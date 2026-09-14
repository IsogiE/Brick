import { existsSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const home = homedir();
const purge = process.argv.includes('--purge');
const paths = [
  join(home, '.local/bin/Brick.AppImage'),
  join(home, '.local/share/applications/dev.isogi.brick.desktop'),
  join(home, '.local/share/applications/brick.desktop'),
  join(home, '.local/share/icons/hicolor/256x256/apps/dev.isogi.brick.png'),
  join(home, '.local/share/icons/hicolor/256x256/apps/brick.png'),
  join(home, '.config/autostart/Brick.desktop'),
  join(home, '.config/autostart/dev.isogi.brick.desktop')
];

if (purge) {
  // Only the native app can erase orphaned OS-keyring entries as well as files.
  // Complete that device-only reset before removing the executable needed to retry.
  const executable = join(home, '.local/bin/Brick.AppImage');
  const probe = spawnSync(executable, ['--local-erasure-protocol'], {
    encoding: 'utf8', timeout: 10000, maxBuffer: 4096
  });
  if (probe.status !== 0 || probe.stdout.trim() !== 'BRICK-LOCAL-ERASURE-v1') {
    throw new Error('Install a current Brick AppImage before purging app data. Nothing was removed.');
  }
  console.log('Resetting all Brick accounts on this device. This does not revoke remote access or delete server data.');
  const reset = spawnSync(executable, ['--purge-local-data'], {
    stdio: 'inherit', timeout: 120000
  });
  if (reset.status !== 0) {
    throw new Error('Brick local data removal did not finish. The app was kept so you can retry.');
  }
}

for (const path of paths) {
  if (existsSync(path)) {
    rmSync(path, { recursive: true, force: true });
    console.log(`Removed ${path}`);
  }
}

spawnSync('update-desktop-database', [join(home, '.local/share/applications')], {
  stdio: 'ignore'
});
spawnSync('gtk-update-icon-cache', ['-f', '-t', join(home, '.local/share/icons/hicolor')], {
  stdio: 'ignore'
});

console.log(purge ? 'Brick local install and app data removed.' : 'Brick local launcher install removed.');
