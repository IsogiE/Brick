import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from timestamp_youtube import extraction_failure, youtube_fragments, youtube_media_start, youtube_session


class YouTubeSessionTests(unittest.TestCase):
    def test_segment_clock_locates_the_screenshot_pull_without_rebasing_media_time(self):
        def segment(ingestion=1789058960054776, duration=5000000):
            payload = (b'\x00'*4 + b'http://youtube.com/streaming/metadata/segment/102015\x00\x00'
                       + b'\x00'*16 + f'Sequence-Number: 538\r\nIngestion-Walltime-Us: {ingestion}\r\nTarget-Duration-Us: {duration}\r\n'.encode())
            return (len(payload)+8).to_bytes(4,'big')+b'emsg'+payload
        start=2695.468
        self.assertAlmostEqual(youtube_media_start(segment(),start,1789058694.468,5),2424.413224,places=5)
        for invalid in [b'', b'garbage', segment()[:20], segment(1), segment(duration=1000000), segment(1789078960054776)]:
            self.assertEqual(youtube_media_start(invalid,start,1789058694.468,5),start)

    def test_live_range_covers_the_scan_without_starting_the_unbounded_generator(self):
        def unexpected_generator(*args):
            self.fail('live generator must not run')
        for duration in [1, 5, 10]:
            selected = {'protocol': 'http_dash_segments_generator', 'target_duration': duration,
                        'url': 'https://rr1.googlevideo.com/videoplayback?signature=fixture&sq=999',
                        'fragments': unexpected_generator}
            fragments = youtube_fragments(selected, 3970.25)
            sequences = [int(parse_qs(urlsplit(f['url']).query)['sq'][0]) for f in fragments]
            self.assertEqual(sequences, list(range(sequences[0], sequences[-1] + 1)))
            self.assertLessEqual(sequences[0] * duration, 3970.25)
            self.assertGreaterEqual((sequences[-1] + 1) * duration, 3970.25 + 28)
            self.assertLessEqual(len(fragments), 30)
            self.assertEqual(parse_qs(urlsplit(fragments[0]['url']).query)['signature'], ['fixture'])

    def test_archived_range_stays_bounded_and_invalid_live_hosts_are_rejected(self):
        selected = {'target_duration': 5, 'fragments': list(range(100))}
        self.assertEqual(youtube_fragments(selected, 51), list(range(9, 16)))
        for url in ['http://rr1.googlevideo.com/media', 'https://googlevideo.com.evil.test/media', 'https://rr1.googlevideo.com:8443/media']:
            self.assertEqual(youtube_fragments({'protocol': 'http_dash_segments_generator', 'target_duration': 5, 'url': url}, 50), [])
        for duration in [0, 11, float('nan')]:
            self.assertEqual(youtube_fragments({**selected, 'target_duration': duration}, 50), [])

    def test_session_copy_is_private_disposable_and_leaves_source_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'session.txt'
            original = b'# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tTEST\tfixture\n'
            source.write_bytes(original)
            source.chmod(0o400)
            with patch.dict(os.environ, {'BRICK_YOUTUBE_COOKIES_FILE': str(source)}):
                with self.assertRaisesRegex(RuntimeError, 'extraction failed'):
                    with youtube_session('youtube') as options:
                        copied = Path(options['cookiefile'])
                        self.assertEqual(copied.read_bytes(), original)
                        self.assertEqual(copied.stat().st_mode & 0o777, 0o600)
                        copied.write_text('yt-dlp updated this copy')
                        raise RuntimeError('extraction failed')
                self.assertFalse(copied.exists())
                self.assertEqual(source.read_bytes(), original)

    def test_twitch_never_opens_the_youtube_session(self):
        with patch.dict(os.environ, {'BRICK_YOUTUBE_COOKIES_FILE': '/missing/session.txt'}):
            with youtube_session('twitch') as options:
                self.assertEqual(options, {})

    def test_unconfigured_youtube_remains_anonymous(self):
        with patch.dict(os.environ, {}, clear=True):
            with youtube_session('youtube') as options:
                self.assertEqual(options, {})

    def test_rejects_missing_empty_oversized_and_linked_sessions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'empty').touch()
            (root / 'large').write_bytes(b'x' * (1024 * 1024 + 1))
            (root / 'linked').symlink_to(root / 'large')
            for name in ['missing', 'empty', 'large', 'linked']:
                with self.subTest(name=name), patch.dict(os.environ, {'BRICK_YOUTUBE_COOKIES_FILE': str(root / name)}):
                    with self.assertRaises((OSError, ValueError)):
                        with youtube_session('youtube'):
                            self.fail('invalid session was accepted')

    def test_failure_status_never_contains_exception_urls_or_session_material(self):
        self.assertEqual(extraction_failure(Exception("Sign in to confirm you're not a bot. https://example.com/?cookie=secret")),
                         {'error': 'youtube_auth_required'})
        self.assertEqual(extraction_failure(Exception('HTTP Error 429: Too Many Requests')),
                         {'error': 'provider_rate_limited'})
        self.assertIsNone(extraction_failure(Exception('https://example.com/?cookie=secret')))


if __name__ == '__main__':
    unittest.main()
