"""Read completed PNGs as they arrive, retaining only the reader's numeric edge."""
import json
from pathlib import Path
import selectors
import subprocess
import time


def read_frames(command, directory, reader, start_ms, at, scale, clock_offset, locate, count):
    producer = consumer = None
    selector = selectors.DefaultSelector()
    try:
        consumer = subprocess.Popen([reader, '--stream'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                    stderr=subprocess.DEVNULL, text=True)
        consumer.stdin.write(json.dumps({'startMs': start_ms, 'locate': locate}) + '\n')
        consumer.stdin.flush()
        selector.register(consumer.stdout, selectors.EVENT_READ)
        producer = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 60
        processed = 0
        previous = None
        while True:
            finished = producer.poll()
            if finished not in (None, 0): return None
            if finished is None and time.monotonic() > deadline: return None
            # ffmpeg's atomic_writing option exposes only complete PNGs. Each
            # acknowledgement means both its pixels and edge evidence were read.
            frames = sorted(directory.glob('frame-*.png'))
            if processed + len(frames) > (100 if locate else count): return None
            if sum(p.stat().st_size for p in frames) > 512 * 1024 * 1024: return None
            for path in frames:
                seconds = int(path.stem.split('-')[1]) / scale + clock_offset
                if not max(0, at-.2) <= seconds <= at+(21 if locate else count/10+1): return None
                if previous is not None and seconds <= previous: return None
                if not 0 < path.stat().st_size <= 2 * 1024 * 1024: return None
                consumer.stdin.write(json.dumps({'path': str(path), 'seconds': seconds}) + '\n')
                consumer.stdin.flush()
                if not selector.select(timeout=30): return None
                reply = consumer.stdout.readline(4097)
                if len(reply) > 4096 or not reply.endswith('\n'): return None
                value = json.loads(reply)
                # The persistent reader retains marker coordinates and edge
                # times, so even positive/uncertain frames need no image history.
                path.unlink()
                previous = seconds
                processed += 1
                if 'result' in value: return value['result']
                if value != {'continue': True}: return None
            if finished == 0: return None
            if not frames: time.sleep(.02)
    finally:
        selector.close()
        for process in (producer, consumer):
            if process is not None:
                if process.poll() is None: process.kill()
                process.wait(timeout=5)
                for pipe in (process.stdin, process.stdout):
                    if pipe is not None: pipe.close()
