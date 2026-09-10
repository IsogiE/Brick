"""Bounded public-video scan. Only the numeric alignment reaches the database."""
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.request
from urllib.parse import urlsplit, urljoin
import yt_dlp
from timestamp_frames import read_frames
from timestamp_youtube import extraction_failure, youtube_fragments, youtube_media_start, youtube_session

class Quiet:
    def debug(self, *_): pass
    def warning(self, *_): pass
    def error(self, *_): pass

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs): return None

def twitch_fragment_video(source, start, directory):
    opener = urllib.request.build_opener(NoRedirect())
    def get(url, limit):
        address = urlsplit(url)
        if address.scheme != 'https' or not (address.hostname or '').endswith(('.ttvnw.net', '.cloudfront.net', '.twitchcdn.net')) or address.port not in (None, 443):
            raise ValueError('Unexpected media host')
        with opener.open(url, timeout=5) as response:
            body = response.read(limit+1)
        if len(body)>limit: raise ValueError('Media limit')
        return body
    manifest = get(source, 2*1024*1024).decode('utf-8')
    segments, elapsed, duration, initialization = [], 0.0, None, None
    for line in manifest.splitlines():
        line = line.strip()
        if line.startswith(('#EXT-X-KEY:', '#EXT-X-BYTERANGE:')): return None
        if line.startswith('#EXT-X-MAP:'):
            match = re.search(r'URI="([^"]+)"', line)
            if not match: return None
            initialization = urljoin(source, match[1])
        elif line.startswith('#EXTINF:'):
            duration = float(line.split(':',1)[1].split(',')[0])
            if not math.isfinite(duration) or not 0<duration<=20: return None
        elif line and not line.startswith('#') and duration is not None:
            segments.append((elapsed, duration, urljoin(source,line)))
            elapsed += duration; duration = None
    chosen = [s for s in segments if s[0]+s[1]>start and s[0]<start+28]
    if not chosen or len(chosen)>16: return None
    path = directory/('source.mp4' if initialization else 'source.ts')
    total = 0
    with path.open('wb') as target:
        if initialization: target.write(get(initialization, 4*1024*1024))
        for _, _, url in chosen:
            body = get(url, 16*1024*1024); total += len(body)
            if total>80*1024*1024: return None
            target.write(body)
    # Preserve the player's playlist clock while decoding only this short
    # fragment range. Local media avoids provider HLS deep-seek differences.
    return str(path), start-chosen[0][0], chosen[0][0]

def fragment_video(selected, start, directory, wall_start=None):
    opener = urllib.request.build_opener(NoRedirect())
    total = 0
    cache = {}
    def get(fragment):
        nonlocal total
        url = fragment.get('url', '')
        address = urlsplit(url)
        if address.scheme != 'https' or not (address.hostname or '').endswith('.googlevideo.com') or address.port not in (None, 443):
            raise ValueError('Unexpected media host')
        if url not in cache:
            with opener.open(url, timeout=5) as response:
                body = response.read(8 * 1024 * 1024 + 1)
            total += len(body)
            if len(body) > 8 * 1024 * 1024 or total > 48 * 1024 * 1024:
                raise ValueError('Media limit')
            cache[url] = body
        return cache[url]
    # Live renditions can start after the broadcast announcement (or restart).
    # Locate the pull with YouTube's segment clock before decoding its marker.
    # At most two probes account for a discontinuity across the initial range.
    if wall_start is not None:
        for _ in range(2):
            chosen = youtube_fragments(selected, start)
            if not chosen: return None
            corrected = youtube_media_start(get(chosen[0]), start, wall_start, selected['target_duration'])
            if abs(corrected-start) < .5:
                start = corrected
                break
            start = corrected
    chosen = youtube_fragments(selected, start)
    if not chosen: return None
    path = directory / 'source.mp4'
    with path.open('wb') as target:
        for fragment in chosen: target.write(get(fragment))
    cache.clear()
    probe = subprocess.run(['ffprobe', '-v', 'error', '-show_entries', 'format=start_time', '-of', 'json', str(path)],
                           capture_output=True, text=True, timeout=5)
    base = float(json.loads(probe.stdout)['format']['start_time'])
    if not math.isfinite(base) or not max(0, start-25) <= base <= start: return None
    # Preserve the native player media clock. Wall time selects the range;
    # only the verified visual five-second edge supplies the final alignment.
    return str(path), start-base, start

def scan(key, reader, attempt=1):
    provider, video = key['provider'], key['videoId']
    if provider == 'twitch' and re.fullmatch(r'[0-9]{1,32}', video):
        page = 'https://www.twitch.tv/videos/' + video
    elif provider == 'youtube' and re.fullmatch(r'[a-zA-Z0-9_-]{11}', video):
        page = 'https://www.youtube.com/watch?v=' + video
    else:
        return None
    estimate = (key['startMs'] - key['recordingStartMs']) / 1000
    if not math.isfinite(estimate) or not 0 <= estimate <= 604800:
        return None
    with youtube_session(provider) as session, yt_dlp.YoutubeDL({'quiet': True, 'logger': Quiet(), 'skip_download': True,
                          'socket_timeout': 10, 'retries': 0, 'extractor_retries': 0,
                          'noplaylist': True, 'cachedir': False, 'js_runtimes': {'node': {}},
                          'live_from_start': provider == 'youtube', **session}) as downloader:
        info = downloader.extract_info(page, download=False)
    formats = [f for f in info.get('formats', []) if f.get('height') and 720 <= f['height'] <= 2160
               and f.get('url') and f.get('vcodec') != 'none']
    if not formats:
        return None
    formats.sort(key=lambda f: (provider == 'youtube' and info.get('is_live') and not f.get('is_from_start'),
                               abs(f['height'] - 1080), f.get('fps') or 60))
    source = formats[0]['url']
    address = urlsplit(source)
    allowed = ('.ttvnw.net', '.cloudfront.net', '.twitchcdn.net') if provider == 'twitch' else ('.googlevideo.com', '.youtube.com')
    if address.scheme != 'https' or not (address.hostname or '').endswith(allowed) or address.port not in (None, 443):
        return None
    offset = {1: -7.5, 2: -25, 3: 10}.get(attempt, -7.5)
    start = max(0, estimate + offset)
    with tempfile.TemporaryDirectory(prefix='brick-frames-') as temporary:
        directory = Path(temporary)
        timeline = ['-copyts', '-start_at_zero']
        protocols = 'https,tcp,tls,crypto'
        seek = start
        clock_offset = 0
        if provider == 'twitch':
            fragments = twitch_fragment_video(source, start, directory)
            if fragments is None: return None
            source, seek, clock_offset = fragments
            protocols = 'file,crypto,data'
        if provider == 'youtube' and formats[0].get('protocol') in ('http_dash_segments', 'http_dash_segments_generator'):
            fragments = fragment_video(formats[0], start, directory, key['startMs']/1000+offset)
            if fragments is None: return None
            source, seek, start = fragments
            timeline = ['-copyts']
            protocols = 'file'
        input_origin = start-seek
        def extract(at, locate, count=70):
            for old in directory.glob('frame-*.png'): old.unlink()
            scale = 1000 if locate else 10
            crop = 'crop=min(iw/2\\,960):min(ih/3\\,360):0:0'
            filters = f'trim=end={at-clock_offset+20},' + crop if locate else crop + ',fps=10:round=near'
            command = ['ffmpeg', '-nostdin', '-hide_banner', '-loglevel', 'error', '-threads', '1',
                       '-filter_threads', '1', '-rw_timeout', '5000000',
                       '-protocol_whitelist', protocols, *timeline,
                       *(['-skip_frame', 'nokey'] if locate else []),
                       '-ss', str(max(0,at-input_origin)), '-t', str(22 if locate else count/10+3), '-i', source, '-map', '0:v:0', '-an', '-sn', '-dn',
                       '-vf', filters, '-frames:v', '100' if locate else str(count),
                       '-fps_mode', 'passthrough', '-enc_time_base', '1:'+str(scale),
                       '-threads', '1', '-frame_pts', '1', '-atomic_writing', '1', str(directory / 'frame-%012d.png')]
            return read_frames(command, directory, reader, key['startMs'], at,
                               scale, clock_offset, locate, count)
        # Locate the five-second marker using only keyframes, then decode a
        # short continuous range to measure its edge with the same reader.
        located = extract(start, True)
        if not isinstance(located,dict) or not isinstance(located.get('foundSeconds'),(int,float)):
            return extract(start, False, 200) if provider == 'youtube' else None
        return extract(max(0,located['foundSeconds']-.5), False)

if __name__ == '__main__':
    try:
        request = json.loads(sys.stdin.buffer.read(4097))
        value = scan(request['key'], os.environ.get('BRICK_TIMESTAMP_READER', '/app/brick-timestamp-reader'), request.get('attempt', 1))
    except Exception as error:
        value = extraction_failure(error)
    print(json.dumps(value))
