import { generateKeyPairSync } from 'node:crypto';

const { publicKey, privateKey } = generateKeyPairSync('ed25519');
const publicDer = publicKey.export({ format: 'der', type: 'spki' });
const privateDer = privateKey.export({ format: 'der', type: 'pkcs8' });

console.log('Store these as GitHub repository secrets:');
console.log(`BRICK_ADDON_PRIVATE_KEY_B64=${privateDer.toString('base64')}`);
console.log('');
console.log('Build the app with this value embedded:');
console.log(`BRICK_ADDON_PUBLIC_KEY_B64=${publicDer.subarray(-32).toString('base64')}`);
