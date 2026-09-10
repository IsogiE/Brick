import json
from pathlib import Path
import sys
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch
from timestamp_frames import read_frames


def fixture_frames(*args):
    # Test scripts run through Python so production's noexec scratch mount
    # stays enabled. The actual reader is an executable in the read-only image.
    reader = args[2]
    popen = subprocess.Popen
    def launch(command, **kwargs):
        if command[0] == reader: command = [sys.executable, *command]
        return popen(command, **kwargs)
    with patch('timestamp_frames.subprocess.Popen', side_effect=launch):
        return read_frames(*args)


class FrameTests(unittest.TestCase):
    def test_reads_and_discards_frames_before_capture_finishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reader = root/'reader'
            reader.write_text('#!' + sys.executable + '\n' + textwrap.dedent('''\
                import json,sys
                header=json.loads(sys.stdin.readline())
                for i,line in enumerate(sys.stdin):
                    frame=json.loads(line)
                    assert open(frame['path']).read()=='complete'
                    print(json.dumps({'result':{'videoSeconds':1.25}} if i==64 else {'continue':True}),flush=True)
            '''))
            reader.chmod(0o700)
            # The producer cannot finish unless its previous frame is consumed
            # and unlinked. This catches a regression to capture-then-read.
            producer = textwrap.dedent('''\
                import pathlib,sys,time
                root=pathlib.Path(sys.argv[1])
                for i in range(200):
                    p=root/f'frame-{i:012d}.png'
                    temporary=p.with_suffix('.png.tmp')
                    temporary.write_text('complete');temporary.rename(p)
                    end=time.monotonic()+4
                    while p.exists():
                        if time.monotonic()>end: sys.exit(3)
                        time.sleep(.002)
            ''')
            result = fixture_frames([sys.executable,'-c',producer,str(root)], root, str(reader),
                                 1789060078000, 0, 10, 0, False, 200)
            self.assertEqual(result, {'videoSeconds':1.25})
            self.assertFalse((root/'frame-000000000064.png').exists())
            self.assertFalse((root/'frame-000000000199.png').exists())

    def test_partial_files_and_failed_capture_cannot_produce_alignment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            reader=root/'reader'
            reader.write_text('#!'+sys.executable+'\nimport time\ntime.sleep(5)\n')
            reader.chmod(0o700)
            (root/'frame-000000000000.png.tmp').write_text('unfinished')
            self.assertIsNone(fixture_frames([sys.executable,'-c','raise SystemExit(1)'], root,
                                          str(reader),1789060078000,0,10,0,False,200))

if __name__ == '__main__': unittest.main()
