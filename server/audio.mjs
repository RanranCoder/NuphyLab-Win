import {randomUUID} from 'node:crypto';
import {AudioReactive,validateAudioFeature} from '../shared/audio-reactive.mjs';
import {ScreenAmbient,validateScreenFeature,validateScreenSettings} from '../shared/screen-ambient.mjs';
const AUDIO_ERRORS=['microphonePermission','inputUnavailable','systemCapture','systemFormat','systemPermission','systemUnsupported'];
const SCREEN_ERRORS=['screenCapture','screenPermission'];
export class AudioSession {
  constructor(now=()=>performance.now()) {
    this.now=now;this.renderer=new AudioReactive();this.screen=new ScreenAmbient();
    this.session=null;this.status='idle';this.error=null;this.source='system';
    this.native={available:false,systemSupported:false,devices:[],microphonePermission:false};this.nativeAt=-Infinity;
    this.lastSample=-Infinity;this.lastPacket=-Infinity;this.startedAt=0;this.paused=false;
  }
  inventory(value) {
    if(typeof value.systemSupported!=='boolean' || !Array.isArray(value.devices) || value.devices.length>100 || value.devices.some(d=>typeof d.id!=='string'||d.id.length>256||typeof d.name!=='string'||d.name.length>256))throw new Error('声音设备列表无效');
    this.native={available:true,systemSupported:value.systemSupported,devices:value.devices.map(d=>({id:d.id,name:d.name})),microphonePermission:value.microphonePermission===true,screenSupported:value.screenSupported===true};this.nativeAt=this.now();
  }
  start(config) {
    if(this.now()-this.nativeAt>12000)throw new Error('请在 NuPhyLab 桌面应用中使用声音律动');
    // Screen capture has its own capability flag: the Windows shell reports it,
    // and the server refuses it elsewhere so a stale UI cannot start it.
    if(config.source==='screen'&&!this.native.screenSupported)throw new Error('屏幕取色需要 Windows 桌面版');
    // The Windows shell reports systemSupported unconditionally (loopback has
    // no OS gate there), so this branch is macOS-relevant only; the text
    // stays platform-specific.
    if(config.source==='system'&&!this.native.systemSupported)throw new Error(process.platform==='win32'?'系统声音暂不可用，请改用麦克风':'系统声音需要 macOS 14.2 或更新版本，请改用麦克风');
    if(config.source==='microphone'&&config.deviceId&&!this.native.devices.some(d=>d.id===config.deviceId))throw new Error('所选麦克风已断开，请重新选择');
    this.session=randomUUID();this.status='starting';this.error=null;this.source=config.source;this.startedAt=this.now();this.lastPacket=this.now();this.lastSample=-Infinity;this.paused=false;
    this.renderer.reset();this.screen.reset();
  }
  stop(error=null){this.session=null;this.paused=false;this.status=error?'error':'idle';this.error=error;this.renderer.reset();this.screen.reset();this.lastSample=-Infinity}
  ingest(packet) {
    if(!this.session || packet.session!==this.session || this.paused)return false;
    if(this.source==='screen')return this.ingestScreen(packet);
    if(!['requesting','listening','error'].includes(packet.status))throw new Error('声音采集状态无效');
    if(packet.status==='error') {this.error=AUDIO_ERRORS.includes(packet.error)?packet.error:'systemCapture';this.status='error';return true}
    if(this.error)return false;
    if(packet.rms!==undefined){this.renderer.feed(validateAudioFeature(packet));this.lastSample=this.now()}
    this.status=packet.status;this.lastPacket=this.now();return true;
  }
  /** Screen packets share the audio session lifecycle but carry zone colors. */
  ingestScreen(packet) {
    if(!['requesting','listening','error'].includes(packet.status))throw new Error('屏幕采集状态无效');
    // An audio packet routed to a screen session (or vice versa) is a bug in
    // the shell, not something to render: refuse it loudly.
    if(packet.rms!==undefined || packet.bands!==undefined)throw new Error('屏幕取色数据无效');
    if(packet.status==='error') {this.error=SCREEN_ERRORS.includes(packet.error)?packet.error:'screenCapture';this.status='error';return true}
    if(this.error)return false;
    if(packet.zones!==undefined){this.screen.feed(validateScreenFeature(packet));this.lastSample=this.now()}
    this.status=packet.status;this.lastPacket=this.now();return true;
  }
  suspend(paused){
    if(paused===this.paused)return;
    this.paused=paused;this.renderer.reset();this.screen.reset();this.lastSample=-Infinity;this.lastPacket=this.now();
    if(!paused){this.status='starting';this.startedAt=this.now()}
  }
  expired(){return this.session&&!this.paused&&(this.error || this.now()-this.nativeAt>12000 || (this.status==='listening'&&this.now()-this.lastPacket>6000) || this.now()-this.startedAt>120000&&this.status!=='listening')}
  snapshot(config) {
    const screen=config?.source==='screen';
    const summary=screen?this.screen.summary():null;
    return {active:!!this.session,session:this.session,status:this.paused?'paused':this.status,error:this.error,
      ...(screen?{average:summary?summary.average:null,saturation:summary?summary.saturation:0}:{level:this.renderer.level,bands:this.renderer.bands}),
      ...this.native,available:this.native.available&&this.now()-this.nativeAt<12000,
      screenSupported:this.native.screenSupported===true,
      capture:{enabled:!!this.session&&!this.paused&&!this.error,session:this.session,source:config.source,deviceId:config.deviceId}};
  }
  render(config){
    if(config.source==='screen'){
      // A static screen stops zone packets but the capture thread keeps sending
      // listening keepalives every second. Staleness for the fade-out must
      // therefore track the whole packet stream, not the zone updates — a
      // 350 ms zone timeout would fade and relight the keyboard on every idle
      // moment, reading as a breathing overlay on top of the mirror.
      const lapsed=this.now()-this.lastPacket>2500;
      return this.screen.render(validateScreenSettings(config.screen??{}),this.now(),lapsed);
    }
    return this.renderer.render(config,this.now(),this.now()-this.lastSample>350);
  }
}
