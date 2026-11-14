// Screen-ambient lighting: the shell averages the primary monitor into a
// 6x16 zone grid (one cell per keyboard row/column band) and this module turns
// those averages into a full 84-key frame. It is a spatial renderer, not an
// audio one — no beat detector, no FFT bands, only colour that follows what is
// on screen. Pixels never reach this process; only zone averages do.
import {layout,blank} from './layout.mjs';

/** Zone grid the shell produces (matches the keyboard's 6 rows x 16 columns). */
export const SCREEN_ZONES = Object.freeze({rows:6,cols:16});

export const SCREEN_EFFECTS = Object.freeze([
  {id:'mirror',label:'屏幕映射',description:'键盘每个位置显示屏幕上对应区域的颜色。'},
  {id:'average',label:'整体色调',description:'整个键盘显示屏幕的平均颜色，随画面整体变化。'},
  {id:'edge',label:'边缘辉光',description:'键盘外圈跟随屏幕四边的颜色，中间保持暗。'},
]);
const effectIds=new Set(SCREEN_EFFECTS.map(effect=>effect.id));
export const DEFAULT_SCREEN = Object.freeze({effect:'mirror',gain:1.15,smoothing:0.55});

export function validateScreenSettings(input={},previous=DEFAULT_SCREEN) {
  if(!input || typeof input!=='object' || Array.isArray(input)) throw new Error('屏幕取色设置无效');
  const next={...DEFAULT_SCREEN,...previous};
  for(const [key,value] of Object.entries(input)) {
    if(key==='effect' && effectIds.has(value)) next[key]=value;
    else if(key==='gain' && Number.isFinite(value) && value>=0.5 && value<=2) next[key]=value;
    else if(key==='smoothing' && Number.isFinite(value) && value>=0 && value<=0.95) next[key]=value;
    else throw new Error('屏幕取色设置无效');
  }
  return next;
}
/** Validates one shell screenPacket payload: rows x cols RGB triples. */
export function validateScreenFeature(value) {
  const {rows,cols}=SCREEN_ZONES;
  if(!value || value.rows!==rows || value.cols!==cols || !Array.isArray(value.zones) || value.zones.length!==rows*cols)
    throw new Error('屏幕取色数据无效');
  const zones=value.zones.map(zone=>{
    if(!Array.isArray(zone) || zone.length!==3) throw new Error('屏幕取色数据无效');
    const [r,g,b]=zone.map(channel=>{
      if(!Number.isInteger(channel) || channel<0 || channel>255) throw new Error('屏幕取色数据无效');
      return channel;
    });
    return [r,g,b];
  });
  return {rows,cols,zones};
}
const clampChannel=value=>Math.max(0,Math.min(255,Math.round(value)));
function hex(rgb) {
  return '#'+(rgb.map(channel=>clampChannel(channel).toString(16).padStart(2,'0'))).join('');
}
/** Applies gain without washing the zone out. */
function grade(rgb,gain) {
  // A per-channel multiplier clips the brightest channel at 255 and drags the
  // whole zone toward white — a bright screen renders as a pale wash. Subtract
  // the black floor, apply gain, then rescale uniformly so the brightest
  // channel lands at 255 at most: hue and saturation survive, dim zones still
  // brighten, and nothing clips.
  const boosted=rgb.map(channel=>Math.max(0,channel-6)*gain);
  const max=Math.max(...boosted);
  const scale=max>255?255/max:1;
  return boosted.map(channel=>Math.round(channel*scale));
}
function saturationOf(rgb) {
  const max=Math.max(...rgb),min=Math.min(...rgb);
  return max===0?0:(max-min)/max;
}

/**
 * Spatial screen renderer. `feed` receives validated zone averages; `render`
 * smooths them over time (fast attack, slow release, like the audio envelope)
 * and maps them onto keys.
 */
export class ScreenAmbient {
  constructor(){this.reset()}
  reset(){
    this.zones=null;this.smoothed=null;this.lastTime=null;this.sequence=0;this.average=[0,0,0];
  }
  feed(value){
    const {zones}=validateScreenFeature(value);
    this.zones=zones;
    this.sequence++;
    const total=zones.reduce((sum,zone)=>[sum[0]+zone[0],sum[1]+zone[1],sum[2]+zone[2]],[0,0,0]);
    this.average=total.map(sum=>sum/zones.length);
  }
  /** Keyboard cell for a key: its centre determines the zone. */
  zoneOf(key) {
    const col=Math.min(SCREEN_ZONES.cols-1,Math.max(0,Math.floor(key.x+key.w/2)));
    const row=Math.min(SCREEN_ZONES.rows-1,Math.max(0,key.y));
    return row*SCREEN_ZONES.cols+col;
  }
  render(config,now,stale=false) {
    const settings=validateScreenSettings(config);
    const dt=Math.min(250,Math.max(0,this.lastTime===null?50:now-this.lastTime));
    this.lastTime=now;
    const source=this.zones;
    if(!source || stale) {
      // No data yet (or the capture lapsed): fade everything out instead of
      // freezing on a stale frame. The average effect paints `average`
      // directly, so it must decay with the grid.
      if(this.smoothed) this.smoothed=this.smoothed.map(zone=>zone.map(channel=>channel*Math.exp(-dt/220)));
      else this.smoothed=Array(SCREEN_ZONES.rows*SCREEN_ZONES.cols).fill([0,0,0]).map(zone=>[...zone]);
      this.average=this.average.map(channel=>channel*Math.exp(-dt/220));
    } else {
      // smoothing 0 = snap to the new frame, 0.95 = very slow.
      const attack=20+settings.smoothing*380;
      const release=attack*2.6;
      const blended=source.map((zone,index)=>{
        const previous=this.smoothed?.[index] ?? [0,0,0];
        return zone.map((channel,axis)=>{
          const old=previous[axis];
          const tau=channel>old?attack:release;
          return old+(channel-old)*(1-Math.exp(-dt/tau));
        });
      });
      this.smoothed=blended;
    }
    const grid=this.smoothed ?? [];
    const frame=blank();
    for(const key of layout) {
      if(key.index>=83)continue; // the 84th LED is not a key and stays unlit
      let rgb;
      if(settings.effect==='average') {
        rgb=grade(this.average,settings.gain);
      } else if(settings.effect==='edge') {
        // Outer ring follows the screen border nearest to that key; the
        // centre fades out so the effect reads as a glow, not a wash.
        const cx=key.x+key.w/2, cy=key.y+key.h/2;
        const border=Math.min(cx,SCREEN_ZONES.cols-cx,cy,SCREEN_ZONES.rows-cy);
        const zone=grid[this.zoneOf(key)] ?? [0,0,0];
        const falloff=Math.max(0,Math.min(1,(2.6-border)/2.6));
        rgb=grade(zone,settings.gain).map(channel=>channel*falloff);
      } else {
        rgb=grade(grid[this.zoneOf(key)] ?? [0,0,0],settings.gain);
      }
      frame[key.index]=hex(rgb);
    }
    return frame;
  }
  /** Mean colour of the current frame, for the panel's level readout. */
  summary() {
    if(!this.smoothed) return {average:hex(this.average),saturation:saturationOf(this.average)};
    const total=this.smoothed.reduce((sum,zone)=>[sum[0]+zone[0],sum[1]+zone[1],sum[2]+zone[2]],[0,0,0]);
    const mean=total.map(sum=>sum/this.smoothed.length);
    return {average:hex(mean),saturation:saturationOf(mean)};
  }
}
