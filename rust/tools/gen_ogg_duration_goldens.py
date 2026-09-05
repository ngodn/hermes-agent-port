#!/usr/bin/env python3
"""Run Mutagen's complete OggOpus loader against synthetic container cases.

Run with: mise exec uv@0.12.5 -- uv run --no-project --with mutagen==1.47.0
--python 3.12.13 python rust/tools/gen_ogg_duration_goldens.py [--check]
"""
import base64
import io
import json
import struct
import sys
from pathlib import Path
from mutagen.oggopus import OggOpus
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'rust/tools/ogg-duration-goldens.json'
def page(packet, seq, flags=0, pos=0, serial=7, lacing=None):
    if lacing is None: lacing=[255]*(len(packet)//255)+[len(packet)%255]
    return struct.pack('<4sBBqIIIB',b'OggS',0,flags,pos,serial,seq,0,len(lacing))+bytes(lacing)+packet
head=lambda skip=312,rate=48000,version=1,channels=1: b'OpusHead'+struct.pack('<BBHIhB',version,channels,skip,rate,0,0)
tags=b'OpusTags'+struct.pack('<II',0,0)
rows=[]
def add(name,data):
    try: value=OggOpus(io.BytesIO(data)).info.length
    except Exception: value=None
    rows.append({'name':name,'ogg':base64.b64encode(data).decode(),'bits':None if value is None else struct.unpack('<Q',struct.pack('<d',value))[0]})
for skip in [0,312,65535]:
    for pos in [-1,0,312,24000,48000,2928312,2**54+311]:
        add(f'position-{skip}-{pos}',page(head(skip),0,2)+page(tags,1)+page(b'audio',2,4,pos))
for rate in [0,8000,16000,44100,48000]:
    add(f'input-rate-{rate}',page(head(rate=rate),0,2)+page(tags,1)+page(b'audio',2,4,48312))
for version in [0,15,16,255]:
    add(f'version-{version}',page(head(version=version),0,2)+page(tags,1)+page(b'audio',2,4,48312))
base=page(head(),0,2)+page(tags,1)
add('no-bos',page(head(),0)+page(tags,1)+page(b'audio',2,4,48312))
add('no-tags',page(head(),0,2)+page(b'audio',1,4,48312))
add('header-only',page(head(),0,2))
add('tags-only',base)
add('no-finished-tags-position',page(head(),0,2)+page(tags,1,pos=-1)+page(b'x'*255,2,4,-1,lacing=[255]))
add('no-eos',base+page(b'audio',2,0,48312))
add('truncated-tail',base+page(b'audio',2,0,48312)+b'OggSbroken')
add('multiplexed',base+page(b'audio',2,4,48312)+page(b'other',0,4,96000,serial=8))
add('incomplete-final-packet',base+page(b'audio',2,0,48312)+page(b'x'*255,3,4,-1,lacing=[255]))
add('invalid-comments',page(head(),0,2)+page(b'OpusTags',1)+page(b'audio',2,4,48312))
add('leading-stream',page(b'OtherHead',0,2,serial=8)+base+page(b'audio',2,4,48312))
long_tags=b'OpusTags'+struct.pack('<I',300)+b'x'*300+struct.pack('<I',0)
add('split-tags',page(head(),0,2)+page(long_tags[:255],1,lacing=[255])+page(long_tags[255:],2,1)+page(b'audio',3,4,48312))
add('split-tags-sequence-gap',page(head(),0,2)+page(long_tags[:255],1,lacing=[255])+page(long_tags[255:],3,1)+page(b'audio',4,4,48312))
text=json.dumps(rows,indent=2)+'\n'
if sys.argv[1:] == ['--check']: assert OUT.read_text()==text
elif not sys.argv[1:]: OUT.write_text(text)
else: raise SystemExit('usage: gen_ogg_duration_goldens.py [--check]')
print(f'Verified {len(rows)} Ogg Opus files')
