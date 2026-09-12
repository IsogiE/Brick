"""Exercise the worker's supported codecs with synthetic media and no network."""
import argparse
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument('--image', required=True)
args = parser.parse_args()

def run(command):
    result = subprocess.run(command, text=True, capture_output=True, timeout=120)
    if result.returncode:
        raise RuntimeError(f'{command[0]} failed: {result.stderr[-6000:]}')
    return result.stdout

with tempfile.TemporaryDirectory(prefix='brick-media-policy-') as temporary:
    root = Path(temporary)
    root.chmod(0o755)
    codecs = [('h264', 'libx264', 'mp4', ['-preset', 'ultrafast']),
              ('hevc', 'libx265', 'mp4', ['-preset', 'ultrafast', '-x265-params', 'pools=1:frame-threads=1']),
              ('vp8', 'libvpx', 'webm', ['-deadline', 'realtime', '-cpu-used', '8']),
              ('vp9', 'libvpx-vp9', 'webm', ['-deadline', 'realtime', '-cpu-used', '8']),
              ('av1', 'libaom-av1', 'mp4', ['-cpu-used', '8'])]
    for name, encoder, extension, options in codecs:
        run(['ffmpeg', '-nostdin', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i',
             'testsrc2=size=1280x720:rate=10', '-t', '1', '-c:v', encoder, '-threads', '2',
             *options, '-pix_fmt', 'yuv420p', str(root / f'{name}.{extension}')])
    run(['ffmpeg', '-nostdin', '-hide_banner', '-loglevel', 'error', '-i', str(root / 'h264.mp4'),
         '-c', 'copy', '-f', 'mpegts', str(root / 'h264.ts')])
    code = r'''
import json, pathlib, struct, subprocess
version = subprocess.check_output(['ffmpeg', '-version'], text=True)
assert version.startswith('ffmpeg version 9.0.1 '), version
forbidden = {'rasc','cfhd','dirac','png','dvdsub','pgssub','ass'}
decoders = subprocess.check_output(['ffmpeg', '-hide_banner', '-decoders'], text=True)
assert not any(len(line.split()) > 1 and line.split()[1] in forbidden for line in decoders.splitlines())
for source in pathlib.Path('/fixtures').iterdir():
    info = json.loads(subprocess.check_output(['ffprobe','-v','error','-protocol_whitelist','file','-show_entries','stream=codec_name','-of','json',str(source)]))
    assert any(s.get('codec_name') in ['h264','hevc','vp8','vp9','av1'] for s in info['streams'])
    subprocess.run(['ffmpeg','-nostdin','-v','error','-threads','1','-filter_threads','1','-protocol_whitelist','file','-i',str(source),'-map','0:v:0','-an','-sn','-dn','-vf',r'trim=end=1,crop=min(iw/2\,960):min(ih/3\,360):0:0,fps=10:round=near','-frames:v','2','-fps_mode','passthrough','-enc_time_base','1:10','-threads','1','-frame_pts','1','-atomic_writing','1','/tmp/frame-%012d.png'],check=True)
    frames = list(pathlib.Path('/tmp').glob('frame-*.png'))
    assert len(frames) == 2
    for frame in frames:
        assert struct.unpack('>II', frame.read_bytes()[16:24]) == (640, 240)
        frame.unlink()
    print('Verified', source.name)
'''
    print(run(['docker', 'run', '--rm', '--network', 'none', '--read-only', '--cap-drop', 'ALL',
               '--security-opt', 'no-new-privileges:true', '--pids-limit', '64', '--memory', '512m',
               '--tmpfs', '/tmp:rw,noexec,nosuid,nodev,uid=1000,gid=1000,mode=700',
               '--mount', f'type=bind,src={root},dst=/fixtures,readonly',
               '--entrypoint', 'python3', args.image, '-c', code]))
