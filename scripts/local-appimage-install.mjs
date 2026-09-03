import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const home = homedir();
const bundleDir = resolve('dist/packages');
const installPath = join(home, '.local/bin/Brick.AppImage');
const tempInstallPath = join(home, '.local/bin/Brick.AppImage.tmp');
const desktopPath = join(home, '.local/share/applications/dev.isogi.brick.desktop');
const legacyDesktopPath = join(home, '.local/share/applications/brick.desktop');
const iconPath = join(home, '.local/share/icons/hicolor/256x256/apps/dev.isogi.brick.png');
const legacyIconPath = join(home, '.local/share/icons/hicolor/256x256/apps/brick.png');

if (!existsSync(bundleDir)) {
  throw new Error('No AppImage bundle folder found. Run `npm run build:local-appimage` first.');
}

const appImage = findFiles(bundleDir)
  .filter((path) => /brick.*\.AppImage$/i.test(path))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];

if (!appImage) {
  throw new Error('No Brick AppImage found. Run `npm run build:local-appimage` first.');
}

mkdirSync(join(home, '.local/bin'), { recursive: true });
mkdirSync(join(home, '.local/share/applications'), { recursive: true });
mkdirSync(join(home, '.local/share/icons/hicolor/256x256/apps'), { recursive: true });

copyFileSync(appImage, tempInstallPath);
chmodSync(tempInstallPath, 0o755);
renameSync(tempInstallPath, installPath);
copyFileSync(resolve('src/assets/brick.png'), iconPath);
copyFileSync(resolve('src/assets/brick.png'), legacyIconPath);
if (existsSync(legacyDesktopPath)) {
  rmSync(legacyDesktopPath, { force: true });
}

writeFileSync(desktopPath, `[Desktop Entry]
Type=Application
Name=Brick
Comment=Advance Raid Tools
Exec="${installPath}"
Icon=dev.isogi.brick
Terminal=false
Categories=Utility;
StartupNotify=true
StartupWMClass=dev.isogi.brick
`);

spawnSync('update-desktop-database', [join(home, '.local/share/applications')], {
  stdio: 'ignore'
});
spawnSync('gtk-update-icon-cache', ['-f', '-t', join(home, '.local/share/icons/hicolor')], {
  stdio: 'ignore'
});

console.log(`Installed ${appImage}`);
console.log(`Launcher entry: ${desktopPath}`);
console.log('Search your application launcher for "Brick".');

function findFiles(dir) {
  const files = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      files.push(...findFiles(path));
    } else {
      files.push(path);
    }
  }
  return files;
}
