"""Optional worker-only YouTube session, copied into per-job temporary storage."""
from contextlib import contextmanager
import math
import os
from pathlib import Path
import stat
import tempfile
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


def youtube_fragments(selected, start):
    duration = selected.get('target_duration')
    if not isinstance(duration, (int, float)) or not 1 <= duration <= 10:
        return []
    if not math.isfinite(start) or not 0 <= start <= 604800:
        return []
    first = max(0, int(start / duration) - 1)
    end = math.ceil((start + 28) / duration)
    if selected.get('protocol') == 'http_dash_segments_generator':
        # yt-dlp's live generator waits for future fragments indefinitely. Its
        # pinned implementation addresses existing fragments with the sq query
        # parameter; request only this completed pull's short range instead.
        address = urlsplit(selected.get('url', ''))
        if address.scheme != 'https' or not (address.hostname or '').endswith('.googlevideo.com') or address.port not in (None, 443):
            return []
        query = [(k, v) for k, v in parse_qsl(address.query, keep_blank_values=True) if k != 'sq']
        return [{'url': urlunsplit(address._replace(query=urlencode([*query, ('sq', index)])))}
                for index in range(first, end)]
    fragments = selected.get('fragments', [])
    return fragments[first:end] if hasattr(fragments, '__getitem__') else []


@contextmanager
def youtube_session(provider):
    source = os.environ.get('BRICK_YOUTUBE_COOKIES_FILE') if provider == 'youtube' else None
    if not source:
        yield {}
        return
    # yt-dlp updates its cookie jar on close. Keep the mounted session read-only
    # and discard all job-local updates with the existing supervisor cleanup.
    with tempfile.TemporaryDirectory(prefix='brick-youtube-') as temporary:
        descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as handle:
            metadata = os.fstat(handle.fileno())
            if not stat.S_ISREG(metadata.st_mode) or not 0 < metadata.st_size <= 1024 * 1024:
                raise ValueError('Invalid YouTube session file')
            contents = handle.read(1024 * 1024 + 1)
        if len(contents) > 1024 * 1024:
            raise ValueError('Invalid YouTube session file')
        target = Path(temporary) / 'cookies.txt'
        target.write_bytes(contents)
        target.chmod(0o600)
        yield {'cookiefile': str(target)}


def extraction_failure(error):
    message = str(error).lower()
    if 'sign in' in message and ('bot' in message or 'confirm your age' in message):
        return {'error': 'youtube_auth_required'}
    if '429' in message or 'too many requests' in message:
        return {'error': 'provider_rate_limited'}
    return None


def youtube_media_start(body, start, wall_start, duration):
    """Use segment ingestion time only to locate pixels, never as an alignment."""
    if not math.isfinite(wall_start) or not 1 <= duration <= 10:
        return start
    position = 0
    for _ in range(64):
        if position + 8 > len(body): break
        size = int.from_bytes(body[position:position+4], 'big')
        kind = body[position+4:position+8]
        header = 8
        if size == 1:
            if position + 16 > len(body): break
            size = int.from_bytes(body[position+8:position+16], 'big'); header = 16
        if size == 0: size = len(body)-position
        if size < header or position+size > len(body): break
        if kind == b'emsg':
            payload = body[position+header:position+size]
            prefix = b'\x00\x00\x00\x00http://youtube.com/streaming/metadata/segment/102015\x00\x00'
            if payload.startswith(prefix) and len(payload) <= 4096:
                try:
                    fields = dict(line.split(': ', 1) for line in payload[len(prefix)+16:].decode('ascii').splitlines() if ': ' in line)
                    sequence = int(fields['Sequence-Number'])
                    ingestion = int(fields['Ingestion-Walltime-Us']) / 1_000_000
                    segment_duration = int(fields['Target-Duration-Us']) / 1_000_000
                    corrected = sequence*duration + wall_start-ingestion
                    if (0 <= sequence <= 604800 and 1_500_000_000 <= ingestion <= 4_000_000_000
                            and abs(segment_duration-duration) < .001 and 0 <= corrected <= 604800
                            and abs(corrected-start) <= 3600):
                        return corrected
                except (KeyError, ValueError, UnicodeError): pass
        position += size
    return start
