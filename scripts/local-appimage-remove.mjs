import { existsSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const home = homedir();
const purge = process.argv.includes('--purge');
const paths = [
  join(home, '.local/bin/Brick.AppImage'),
  join(home, '.local/share/applications/brick.desktop'),
  join(home, '.local/share/icons/hicolor/256x256/apps/brick.png'),
  join(home, '.config/autostart/Brick.desktop'),
  join(home, '.config/autostart/dev.isogi.brick.desktop')
];

if (purge) {
  paths.push(join(home, '.config/dev.isogi.brick'));
  paths.push(join(home, '.local/share/dev.isogi.brick'));
  paths.push(join(home, '.cache/dev.isogi.brick'));
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

console.log(purge ? 'Brick local install and app data removed.' : 'Brick local launcher install removed.');
