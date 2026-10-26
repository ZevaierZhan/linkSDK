import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {patchLibztPacketAdapter} from './patch-libzt-packet-adapter.mjs';
const root=path.resolve(import.meta.dirname,'..');
const vendor=path.join(root,'vendor/libzt');
const files=['src/Controls.cpp','src/NodeService.cpp','src/NodeService.hpp','src/VirtualTap.cpp','src/Events.cpp','src/Events.hpp','include/ZeroTierSockets.h','pkg/crate/libzt/src/include/ZeroTierSockets.h'];
const fixture=fs.mkdtempSync(path.join(os.tmpdir(),'link-packet-adapter-'));
try {
  for(const file of files){fs.mkdirSync(path.dirname(path.join(fixture,file)),{recursive:true});fs.copyFileSync(path.join(vendor,file),path.join(fixture,file));}
  const snapshot=()=>{const hash=createHash('sha256');for(const file of [...files,'src/LinkPacketAdapter.hpp','src/LinkPacketAdapter.cpp'])hash.update(fs.readFileSync(path.join(fixture,file)));return hash.digest('hex');};
  patchLibztPacketAdapter(fixture);const once=snapshot();patchLibztPacketAdapter(fixture);assert.equal(snapshot(),once);
  const header=fs.readFileSync(path.join(fixture,'include/ZeroTierSockets.h'),'utf8');
  for(const symbol of ['zts_init_set_packet_adapter','zts_packet_adapter_get_generation','zts_packet_adapter_set_ready','zts_packet_adapter_inject','zts_packet_adapter_stop','zts_packet_adapter_join'])assert.equal(header.match(new RegExp(`ZTS_API int ZTCALL ${symbol}\\(`,'g'))?.length,1);
  const controls=path.join(fixture,'src/Controls.cpp');
  const generationBody=fs.readFileSync(controls,'utf8').split('int zts_packet_adapter_get_generation(uint64_t* generation)')[1].split('int zts_packet_adapter_set_ready')[0];
  assert(generationBody.indexOf('if (generation) *generation = 0;')>=0);
  assert(generationBody.indexOf('if (generation) *generation = 0;')<generationBody.indexOf('if (!generation ||'));
  fs.writeFileSync(controls,fs.readFileSync(controls,'utf8').replace('LINK_EXCLUSIVE_PACKET_ADAPTER','UNREVIEWED_UPSTREAM_CHANGE'));
  assert.throws(()=>patchLibztPacketAdapter(fixture),/Pinned libzt packet adapter anchor changed/);
  console.log('Exclusive packet-adapter source patch is idempotent, six C ABI declarations are unique, unreviewed drift fails closed');
} finally {
  assert(path.resolve(fixture).startsWith(path.join(path.resolve(os.tmpdir()),'link-packet-adapter-')));
  fs.rmSync(fixture,{recursive:true,force:true});
}
