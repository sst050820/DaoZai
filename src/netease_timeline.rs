//! 网易云、酷狗未上报系统时间轴时，读取已验证版本的只读播放时间。
use std::ffi::c_void;
type Handle = *mut c_void;
#[link(name="kernel32")]
extern "system" {
    fn OpenProcess(access:u32,inherit:i32,pid:u32)->Handle;
    fn CloseHandle(handle:Handle)->i32;
    fn ReadProcessMemory(handle:Handle,address:usize,buffer:*mut c_void,size:usize,read:*mut usize)->i32;
    fn K32EnumProcesses(ids:*mut u32,size:u32,needed:*mut u32)->i32;
    fn K32EnumProcessModules(handle:Handle,modules:*mut usize,size:u32,needed:*mut u32)->i32;
    fn K32GetModuleBaseNameW(handle:Handle,module:usize,name:*mut u16,size:u32)->u32;
}
struct Process(Handle);
impl Drop for Process {fn drop(&mut self){unsafe{CloseHandle(self.0);}}}
fn read<const N:usize>(p:Handle,address:usize)->Option<[u8;N]> {
    let mut bytes=[0;N];let mut count=0;
    (unsafe{ReadProcessMemory(p,address,bytes.as_mut_ptr().cast(),N,&mut count)}!=0 && count==N).then_some(bytes)
}
fn valid(position:f64,duration:f64)->bool {
    position.is_finite() && duration.is_finite() && duration>0.0 && duration<86400.0 && position>=0.0 && position<=duration
}
#[test]
fn rejects_invalid_timeline(){
    assert!(!valid(f64::NAN,240.0));assert!(!valid(0.0,0.0));assert!(!valid(241.0,240.0));assert!(!valid(-1.0,240.0));assert!(valid(12.01,275.5));
}
fn name(p:Handle,module:usize)->String {
    let mut s=[0u16;260];let n=unsafe{K32GetModuleBaseNameW(p,module,s.as_mut_ptr(),260)} as usize;
    String::from_utf16_lossy(&s[..n])
}
pub fn timeline()->Option<(f64,f64)> {query(false)}
pub fn kugou_position()->Option<f64> {query(true).map(|(position,_)|position)}

fn valid_kugou_position(position:f64)->bool {position.is_finite() && (0.0..86400.0).contains(&position)}
#[test]
fn kugou_rejects_invalid_positions(){
    assert!(valid_kugou_position(0.0));assert!(valid_kugou_position(214.358));
    for invalid in [f64::NAN,f64::INFINITY,-1.0,86400.0]{assert!(!valid_kugou_position(invalid));}
}

/// 已找到的进程与模块基址；每轮只需读几个字节，不再枚举全部进程和模块。
#[derive(Clone,Copy)]
struct Found {pid:u32,base:usize}
struct Slot {found:Option<Found>,missed:Option<std::time::Instant>}
static SLOTS:std::sync::Mutex<[Slot;2]>=std::sync::Mutex::new([Slot{found:None,missed:None},Slot{found:None,missed:None}]);
/// 没找到（未运行或版本不匹配）时，重新枚举的最短间隔。
const REDISCOVER:std::time::Duration=std::time::Duration::from_secs(3);

fn target(kugou:bool)->(&'static str,&'static str,u32,u32,usize){
    if kugou {("KuGou.exe","kgplayer.dll",0x6ac9ec86,4587520,0x3949f8)}else{("cloudmusic.exe","cloudmusic.dll",0x6ad712e1,41054208,0x21a74d8)}
}

fn query(kugou:bool)->Option<(f64,f64)> {
    let mut slots=SLOTS.lock().unwrap_or_else(|e|e.into_inner());
    let slot=&mut slots[kugou as usize];
    if let Some(found)=slot.found {
        if let Some(value)=sample(kugou,found){return value;}
        slot.found=None;
    }
    if slot.missed.is_some_and(|at|at.elapsed()<REDISCOVER){return None;}
    match discover(kugou) {
        Some(found)=>{slot.found=Some(found);slot.missed=None;sample(kugou,found).flatten()}
        None=>{slot.missed=Some(std::time::Instant::now());None}
    }
}

/// 外层 None 表示进程或模块已失效，需要重新查找；内层 None 只是本轮读数无效。
fn sample(kugou:bool,found:Found)->Option<Option<(f64,f64)>> {
    let (process,_,stamp,image_size,position_offset)=target(kugou);
    let p=Process(unsafe{OpenProcess(0x410,0,found.pid)});if p.0.is_null(){return None;}
    // 进程号可能被复用：每轮都核对进程名与 PE 标识（只读几个字节）。
    if !name(p.0,0).eq_ignore_ascii_case(process){return None;}
    let pe=u32::from_le_bytes(read::<4>(p.0,found.base+60)?) as usize;
    if pe>4096{return None;}
    if u32::from_le_bytes(read::<4>(p.0,found.base+pe+8)?)!=stamp || u32::from_le_bytes(read::<4>(p.0,found.base+pe+80)?)!=image_size{return None;}
    let position=f64::from_le_bytes(read::<8>(p.0,found.base+position_offset)?);
    if kugou {return Some(valid_kugou_position(position).then_some((position,0.0)));}
    let duration=f64::from_le_bytes(read::<8>(p.0,found.base+0x2214778)?);
    Some(valid(position,duration).then_some((position,duration)))
}

fn discover(kugou:bool)->Option<Found> {
    let (process,module,stamp,image_size,_)=target(kugou);
    let mut ids=[0u32;4096];let mut needed=0;
    if unsafe{K32EnumProcesses(ids.as_mut_ptr(),std::mem::size_of_val(&ids) as u32,&mut needed)}==0{return None;}
    for &id in ids.iter().take(needed as usize/4) {
        let p=Process(unsafe{OpenProcess(0x410,0,id)});if p.0.is_null(){continue;}
        if !name(p.0,0).eq_ignore_ascii_case(process){continue;}
        let mut modules=[0usize;1024];let mut size=0;
        if unsafe{K32EnumProcessModules(p.0,modules.as_mut_ptr(),std::mem::size_of_val(&modules) as u32,&mut size)}==0{continue;}
        for &base in modules.iter().take(size as usize/std::mem::size_of::<usize>()) {
            if !name(p.0,base).eq_ignore_ascii_case(module){continue;}
            let pe=u32::from_le_bytes(read::<4>(p.0,base+60)?) as usize;
            if pe>4096{return None;}
            // 仅使用本机验证过的 PE 时间戳与映像大小，其他版本安全回退。
            if u32::from_le_bytes(read::<4>(p.0,base+pe+8)?)!=stamp || u32::from_le_bytes(read::<4>(p.0,base+pe+80)?)!=image_size{return None;}
            return Some(Found{pid:id,base});
        }
    }
    None
}
