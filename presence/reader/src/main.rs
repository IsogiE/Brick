#![allow(dead_code)]
#[path = "../../../src/replay_digits.rs"]
mod replay_digits;
#[path = "../../../src/replay_edge.rs"]
mod replay_edge;
use serde::Deserialize;
use std::{
    fs::File,
    io::{self, BufRead, Read, Write},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    start_ms: i64,
    #[serde(default)]
    locate: bool,
}
#[derive(Deserialize)]
struct Request {
    #[serde(flatten)]
    header: Header,
    frames: Vec<Frame>,
}
#[derive(Deserialize)]
struct Frame {
    path: String,
    seconds: f64,
}
struct Reader {
    header: Header,
    edge: replay_edge::Edge,
    previous: Option<f64>,
}
impl Reader {
    fn new(header: Header) -> Result<Self, ()> {
        if !(1_500_000_000_000..=4_000_000_000_000).contains(&header.start_ms) {
            return Err(());
        }
        Ok(Self {
            header,
            edge: replay_edge::Edge::default(),
            previous: None,
        })
    }
    fn frame(&mut self, frame: Frame) -> Result<Option<serde_json::Value>, ()> {
        if !frame.seconds.is_finite() || !(0.0..=604800.0).contains(&frame.seconds) {
            return Err(());
        }
        if self.previous.is_some_and(|p| {
            frame.seconds <= p || (!self.header.locate && frame.seconds - p > 0.20)
        }) {
            return Err(());
        }
        self.previous = Some(frame.seconds);
        let mut png = Vec::new();
        File::open(frame.path)
            .map_err(|_| ())?
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut png)
            .map_err(|_| ())?;
        if png.len() > 2 * 1024 * 1024 {
            return Err(());
        }
        let reading = replay_digits::read(&png, self.header.start_ms, self.edge.marker);
        if self.header.locate {
            return Ok(matches!(reading, replay_digits::Reading::Present(_))
                .then(|| serde_json::json!({"foundSeconds": frame.seconds})));
        }
        // Keep only numeric edge evidence between frames, never decoded images.
        self.edge
            .observe(replay_edge::Sample {
                reading,
                before: (frame.seconds - 0.025).max(0.0),
                after: frame.seconds + 0.025,
            })?
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ())
    }
}
fn run() -> Option<serde_json::Value> {
    let mut input = Vec::new();
    io::stdin()
        .take(128 * 1024 + 1)
        .read_to_end(&mut input)
        .ok()?;
    if input.len() > 128 * 1024 {
        return None;
    }
    let request: Request = serde_json::from_slice(&input).ok()?;
    if request.frames.len() > 500 {
        return None;
    }
    let mut reader = Reader::new(request.header).ok()?;
    for frame in request.frames {
        if let Some(result) = reader.frame(frame).ok()? {
            return Some(result);
        }
    }
    None
}
fn line<T: serde::de::DeserializeOwned>(input: &mut impl BufRead) -> Result<T, ()> {
    let mut bytes = Vec::new();
    (&mut *input)
        .take(4097)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > 4096 || bytes.last() != Some(&b'\n') {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}
fn stream(input: &mut impl BufRead, output: &mut impl Write) -> Result<(), ()> {
    let mut reader = Reader::new(line(input)?)?;
    for _ in 0..500 {
        let result = reader.frame(line(input)?)?;
        let reply = match &result {
            Some(value) => serde_json::json!({"result": value}),
            None => serde_json::json!({"continue": true}),
        };
        writeln!(output, "{reply}").map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
        if result.is_some() {
            break;
        }
    }
    Ok(())
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--stream") {
        // One acknowledgement per fully consumed frame lets the supervisor
        // unlink its PNG immediately while this process retains the edge state.
        let _ = stream(&mut io::stdin().lock(), &mut io::stdout().lock());
    } else {
        println!("{}", serde_json::to_string(&run()).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Cursor};
    #[test]
    fn streaming_retains_the_five_second_edge_and_rejects_gaps() {
        let root = std::env::temp_dir().join(format!("brick-reader-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let absent = root.join("absent.png");
        let present = root.join("present.png");
        let unix = 1_789_060_078_i64;
        let mut pixels = image::GrayImage::new(960, 360);
        pixels.save(&absent).unwrap();
        image::imageops::replace(&mut pixels, &replay_digits::template(unix, 16), 6, 6);
        pixels.save(&present).unwrap();
        let header = || Header {
            start_ms: unix * 1000,
            locate: false,
        };
        let mut batch = Reader::new(header()).unwrap();
        let mut input = format!("{{\"startMs\":{},\"locate\":false}}\n", unix * 1000);
        let mut expected = None;
        for i in 0..70 {
            let path = if (10..60).contains(&i) {
                &present
            } else {
                &absent
            };
            let seconds = 10.0 + f64::from(i) / 10.0;
            let frame = Frame {
                path: path.to_string_lossy().into(),
                seconds,
            };
            input += &format!(
                "{}\n",
                serde_json::json!({"path":frame.path,"seconds":seconds})
            );
            if let Some(value) = batch.frame(frame).unwrap() {
                expected = Some(value);
                break;
            }
        }
        let expected = expected.expect("fixture must measure a real five-second marker");
        let mut output = Vec::new();
        stream(&mut Cursor::new(input), &mut output).unwrap();
        let rows = String::from_utf8(output).unwrap();
        let last: serde_json::Value = serde_json::from_str(rows.lines().last().unwrap()).unwrap();
        assert_eq!(last["result"], expected);
        assert!(rows.lines().count() > 50);
        assert!((expected["videoSeconds"].as_f64().unwrap() - 10.95).abs() < 0.001);
        let mut reader = Reader::new(header()).unwrap();
        let frame = |seconds| Frame {
            path: absent.to_string_lossy().into(),
            seconds,
        };
        reader.frame(frame(1.0)).unwrap();
        assert!(reader.frame(frame(1.3)).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn streaming_rejects_oversized_and_unterminated_records() {
        assert!(line::<Header>(&mut Cursor::new(vec![b'x'; 4097])).is_err());
        assert!(line::<Header>(&mut Cursor::new(b"{\"startMs\":1789060078000}")).is_err());
    }
}
