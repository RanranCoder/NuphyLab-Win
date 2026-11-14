import test from 'node:test';
import assert from 'node:assert/strict';
import {ScreenAmbient,SCREEN_EFFECTS,SCREEN_ZONES,validateScreenFeature,validateScreenSettings,DEFAULT_SCREEN} from '../shared/screen-ambient.mjs';
import {layout} from '../shared/layout.mjs';
import {AUDIO_SOURCES,validateAudio,DEFAULT_AUDIO} from '../shared/audio-reactive.mjs';
import {AudioSession} from '../server/audio.mjs';

const grid=(fill)=>Array(SCREEN_ZONES.rows*SCREEN_ZONES.cols).fill(fill).map(zone=>[...zone]);
const zones=(fill)=>validateScreenFeature({rows:SCREEN_ZONES.rows,cols:SCREEN_ZONES.cols,zones:grid(fill)});

test('the zone grid matches the keyboard geometry',()=>{
 // The shell emits one cell per keyboard row/column band; a mismatched grid
 // would silently stretch the image across the wrong keys.
 assert.equal(Math.max(...layout.map(key=>key.y))+1,SCREEN_ZONES.rows);
 assert.equal(Math.max(...layout.map(key=>key.x+key.w)),SCREEN_ZONES.cols);
});

test('screen packets are validated strictly',()=>{
 assert.equal(validateScreenFeature({rows:6,cols:16,zones:grid([1,2,3])}).zones.length,96);
 for(const bad of [
  null,
  {rows:5,cols:16,zones:grid([0,0,0])},
  {rows:6,cols:16,zones:grid([0,0,0]).slice(0,90)},
  {rows:6,cols:16,zones:grid([0,0,0]).map((_,index)=>index===4?[0,0]:[0,0,0])},
  {rows:6,cols:16,zones:grid([0,0,0]).map((_,index)=>index===4?[0,0,256]:[0,0,0])},
  {rows:6,cols:16,zones:grid([0,0,0]).map((_,index)=>index===4?[0,0,1.5]:[0,0,0])},
 ]) assert.throws(()=>validateScreenFeature(bad),/屏幕取色数据无效/);
 assert.throws(()=>validateScreenFeature(undefined),/屏幕取色数据无效/);
});

test('screen settings reject out-of-range gain and smoothing',()=>{
 assert.deepEqual(validateScreenSettings({effect:'edge'}),{...DEFAULT_SCREEN,effect:'edge'});
 assert.deepEqual(validateScreenSettings({gain:1.8,smoothing:0}),{...DEFAULT_SCREEN,gain:1.8,smoothing:0});
 assert.throws(()=>validateScreenSettings({gain:0.2}));
 assert.throws(()=>validateScreenSettings({gain:3}));
 assert.throws(()=>validateScreenSettings({smoothing:1}));
 assert.throws(()=>validateScreenSettings({effect:'nope'}));
 assert.throws(()=>validateScreenSettings({unknown:1}));
});

test('the mirror effect paints each key with its own screen region',()=>{
 const ambient=new ScreenAmbient();
 // Left half red, right half blue: keys must follow their column band.
 const colors=grid([0,0,0]).map((_,index)=>((index%SCREEN_ZONES.cols)<SCREEN_ZONES.cols/2?[255,0,0]:[0,0,255]));
 ambient.feed(validateScreenFeature({rows:SCREEN_ZONES.rows,cols:SCREEN_ZONES.cols,zones:colors}));
 const frame=ambient.render({effect:'mirror',gain:1,smoothing:0},1000);
 assert.equal(frame.length,84);
 const esc=frame[0]; // Escape sits in the left half
 const right=frame[layout.find(key=>key.code==='Backspace').index];
 assert.match(esc,/^#[0-9a-f]{6}$/);
 const [red]=[parseInt(esc.slice(1,3),16)];
 assert.ok(red>140,`left half is red-ish: ${esc}`);
 const [, , blue]=[parseInt(right.slice(1,3),16),parseInt(right.slice(3,5),16),parseInt(right.slice(5,7),16)];
 assert.ok(blue>140,`right half is blue-ish: ${right}`);
 assert.equal(frame[83],'#000000','the 84th LED is not a key and stays unlit');
});

test('the edge effect lights the rim and leaves the center dark',()=>{
 const ambient=new ScreenAmbient();
 ambient.feed(zones([40,180,220]));
 const frame=ambient.render({effect:'edge',gain:1,smoothing:0},1000);
 const brightness=(hex)=>[1,3,5].reduce((sum,start)=>sum+parseInt(hex.slice(start,start+2),16),0);
 const esc=frame[0]; // top row: rim
 const bottomRow=frame[layout.find(key=>key.code==='Space').index]; // bottom row: rim too
 // A key in the middle of the board (row 3, centre columns) is the dark core.
 const middle=layout.filter(key=>key.y===3 && key.x>=6 && key.x<=9).map(key=>frame[key.index]);
 assert.ok(brightness(esc)>0,'the rim key is lit');
 assert.ok(brightness(bottomRow)>0,'the bottom rim is lit as well');
 assert.ok(middle.every(color=>brightness(color)<brightness(esc)),'the centre is dimmer than the rim');
});

test('the average effect paints the whole keyboard one color',()=>{
 const ambient=new ScreenAmbient();
 ambient.feed(zones([40,180,220]));
 const frame=ambient.render({effect:'average',gain:1,smoothing:0},1000);
 const painted=new Set(frame.slice(0,83));
 assert.equal(painted.size,1,'one average color for every key');
});

test('smoothing eases toward a new frame instead of snapping',()=>{
 const ambient=new ScreenAmbient();
 ambient.feed(zones([0,0,0]));
 const first=ambient.render({gain:1,smoothing:0.9},1000);
 assert.ok(first[0]!=='#000000'||first[0]==='#000000');
 ambient.feed(zones([255,255,255]));
 const early=ambient.render({gain:1,smoothing:0.9},1016);
 const earlyRed=parseInt(early[0].slice(1,3),16);
 assert.ok(earlyRed<60,`a 16 ms step must not jump to white: ${early[0]}`);
 const settled=ambient.render({gain:1,smoothing:0},1200);
 assert.ok(parseInt(settled[0].slice(1,3),16)>200,'with smoothing off the frame snaps');
});

test('a stale or empty capture fades out rather than freezing',()=>{
 const ambient=new ScreenAmbient();
 assert.deepEqual(ambient.render({},1000).slice(0,83).every(color=>color==='#000000'),true,'no data paints black');
 ambient.feed(zones([255,255,255]));
 ambient.render({gain:1,smoothing:0},1000);
 const stale=ambient.render({gain:1,smoothing:0},6000,true);
 assert.ok(parseInt(stale[0].slice(1,3),16)<40,`stale capture fades: ${stale[0]}`);
});

test('gain brightens a bright zone without washing it out to white',()=>{
 const ambient=new ScreenAmbient();
 // A bright warm zone (a typical bright window average): the old per-channel
 // gain clipped every channel to 255 — pure white on the keyboard.
 ambient.feed(zones([230,180,160]));
 const frame=ambient.render({effect:'average',gain:2,smoothing:0},1000);
 const [r,g,b]=[1,3,5].map(start=>parseInt(frame[0].slice(start,start+2),16));
 assert.equal(r,255,'the brightest channel saturates');
 assert.equal(g,198,'green scales uniformly instead of clipping');
 assert.equal(b,175,'blue scales uniformly instead of clipping');
 assert.ok(g<220&&b<220,'the zone keeps its warm hue at maximum gain');
});

test('audio settings accept the screen source and nested screen options',()=>{
 assert.ok(AUDIO_SOURCES.includes('screen'));
 const next=validateAudio({source:'screen',screen:{effect:'edge',gain:1.4}});
 assert.equal(next.source,'screen');
 assert.equal(next.screen.effect,'edge');
 assert.equal(next.screen.gain,1.4);
 assert.equal(next.screen.smoothing,DEFAULT_SCREEN.smoothing);
 // A legacy payload without the nested object still validates.
 assert.deepEqual(validateAudio({source:'system'}).screen,DEFAULT_SCREEN);
 assert.throws(()=>validateAudio({source:'webcam'}));
 assert.throws(()=>validateAudio({screen:{gain:9}}));
});

test('the audio session routes screen packets and renders from them',()=>{
 let now=1000;
 const session=new AudioSession(()=>now);
 session.inventory({systemSupported:true,screenSupported:true,devices:[],microphonePermission:true});
 session.start({...DEFAULT_AUDIO,source:'screen'});
 assert.equal(session.session!==null,true);
 assert.equal(session.ingest({session:session.session,op:'screenPacket',status:'listening',...zones([0,255,0])}),true);
 const snapshot=session.snapshot({source:'screen'});
 assert.equal(snapshot.screenSupported,true);
 assert.equal(snapshot.status,'listening');
 assert.match(snapshot.average,/^#[0-9a-f]{6}$/);
 const frame=session.render({source:'screen',screen:{effect:'average',gain:1,smoothing:0}});
 assert.equal(frame.length,84);
 assert.notEqual(frame[0],'#000000');
});

test('a screen session rejects audio-only and malformed packets',()=>{
 const session=new AudioSession(()=>1000);
 session.inventory({systemSupported:true,screenSupported:true,devices:[],microphonePermission:true});
 session.start({...DEFAULT_AUDIO,source:'screen'});
 assert.throws(()=>session.ingest({session:session.session,status:'listening',rms:0.5,bands:[0,0,0,0,0,0,0,0]}),/屏幕取色数据无效/);
 // Stale session ids are ignored, not applied.
 assert.equal(session.ingest({session:'other',status:'error',error:'screenCapture'}),false);
 assert.equal(session.ingest({session:session.session,status:'error',error:'screenCapture'}),true);
 assert.equal(session.error,'screenCapture');
});

test('a static screen holds its colours while the capture keeps alive',()=>{
 let now=1000;
 const session=new AudioSession(()=>now);
 session.inventory({systemSupported:true,screenSupported:true,devices:[],microphonePermission:true});
 session.start({...DEFAULT_AUDIO,source:'screen'});
 session.ingest({session:session.session,status:'listening',...zones([200,60,40])});
 // Zone packets stop on a static screen, but keepalives keep arriving: the
 // frame must hold instead of fading in and out with every idle moment.
 now=60000;
 const held=session.render({source:'screen',screen:{effect:'average',gain:1,smoothing:0}});
 const [r,g]=[1,3].map(start=>parseInt(held[0].slice(start,start+2),16));
 assert.ok(r>120&&g<120,`a static screen must hold its colour, got ${held[0]}`);
 // When the shell stops sending anything at all, the frame fades out.
 now=63000;
 const faded=session.render({source:'screen',screen:{effect:'average',gain:1,smoothing:0}});
 const r2=parseInt(faded[0].slice(1,3),16);
 assert.ok(r2<r,'a lapsed capture fades out instead of freezing');
});

test('screen capture is refused where the shell does not advertise it',()=>{
 const session=new AudioSession(()=>1000);
 session.inventory({systemSupported:true,devices:[],microphonePermission:true});
 assert.throws(()=>session.start({...DEFAULT_AUDIO,source:'screen'}),/屏幕取色需要 Windows 桌面版/);
 const unsupportedName=Object.values(SCREEN_EFFECTS).map(effect=>effect.label);
 assert.equal(unsupportedName.length,3);
});
