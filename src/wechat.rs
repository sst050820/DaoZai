//! 只接收微信任务栏提醒；复用主窗口消息循环，不读取聊天内容。
use std::{cell::RefCell, path::PathBuf, time::{Duration, Instant}};
use windows::{core::{w, PCWSTR, PWSTR}, Win32::{
    Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM},
    System::{Registry::{RegGetValueW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, RRF_RT_REG_SZ},
        Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION}},
    UI::{Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass}, WindowsAndMessaging::*},
}};

const SUBCLASS_ID: usize = 0x5745_4348;
const FLASH: usize = 0x8006;

/// 看过某个账号后切走：这段时间内的闪动算作刚才那次提醒的余波。
const LEAVE_GRACE: Duration = Duration::from_secs(4);

#[derive(Default)]
struct Burst { last: Option<Instant>, quiet_until: Option<Instant> }
impl Burst {
    fn attention(&mut self, now: Instant) -> bool {
        // 刚看完切走：任务栏补闪不算新提醒，但计入这一轮，持续闪动也继续合并。
        if self.quiet_until.is_some_and(|until| now < until) {self.last=Some(now);return false;}
        // 连续闪动只提醒一次；停止闪动五秒后，下一轮才算新提醒。
        self.last.replace(now).is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
    }
    /// 用户刚从这个账号的窗口切走。
    fn left(now: Instant) -> Self { Self{last:Some(now),quiet_until:Some(now+LEAVE_GRACE)} }
}

fn normalized(path: &str) -> String {
    path.trim_start_matches(r"\\?\").replace('/', "\\").to_lowercase()
}

fn registry_string(root:HKEY,key:PCWSTR,name:PCWSTR)->Option<String>{
    let mut data=[0u16;2048];let mut size=(data.len()*2) as u32;
    unsafe {RegGetValueW(root,key,name,RRF_RT_REG_SZ,None,Some(data.as_mut_ptr().cast()),Some(&mut size))}.ok().ok()?;
    let length=data.iter().position(|c|*c==0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..length]))
}

// 只提取协议处理程序的路径，绝不执行注册表里的命令或参数。
fn protocol_executable(command:&str)->Option<PathBuf>{
    let command=command.trim();
    let exe=if let Some(quoted)=command.strip_prefix('"'){quoted.split_once('"')?.0}else{
        command.strip_suffix(" \"%1\"").or_else(||command.strip_suffix(" %1")).unwrap_or(command)
    };
    let path=PathBuf::from(exe);
    let name=path.file_name()?.to_str()?;
    (path.is_absolute() && (name.eq_ignore_ascii_case("Weixin.exe") || name.eq_ignore_ascii_case("WeChat.exe"))).then_some(path)
}

fn installed_paths() -> Vec<String> {
    let mut paths=Vec::new();
    for (key,exe) in [(w!("Software\\Tencent\\Weixin"),"Weixin.exe"),(w!("Software\\Tencent\\WeChat"),"WeChat.exe")] {
        if let Some(folder)=registry_string(HKEY_CURRENT_USER,key,w!("InstallPath")){paths.push(PathBuf::from(folder).join(exe));}
    }
    if let Some(path)=registry_string(HKEY_CLASSES_ROOT,w!("weixin\\shell\\open\\command"),PCWSTR::null()).and_then(|cmd|protocol_executable(&cmd)){paths.push(path);}
    let mut paths:Vec<String>=paths.into_iter().filter(|p|p.is_absolute() && p.is_file()).filter_map(|p|p.canonicalize().ok()).map(|p|normalized(&p.to_string_lossy())).collect();
    // 注册表路径可能过期（升级、换目录、复制一份双开、绿色版）：再认文件名相同、且由腾讯发布的程序。
    paths.extend(["*weixin.exe".to_string(),"*wechat.exe".to_string()]);
    paths
}

/// 程序文件属性里的公司名（结果按路径缓存，避免每次读文件）。
fn company_name(path:&str)->String{
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW,GetFileVersionInfoW,VerQueryValueW};
    static CACHE:std::sync::Mutex<Option<std::collections::HashMap<String,String>>>=std::sync::Mutex::new(None);
    if let Some(name)=CACHE.lock().ok().and_then(|c|c.as_ref().and_then(|c|c.get(path).cloned())){return name;}
    let name=unsafe{(||->Option<String>{
        let wide:Vec<u16>=path.encode_utf16().chain(Some(0)).collect();
        let size=GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()),None);
        if size==0 {return None;}
        let mut data=vec![0u8;size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()),0,size,data.as_mut_ptr().cast()).ok()?;
        let mut pointer=std::ptr::null_mut();let mut length=0u32;
        if !VerQueryValueW(data.as_ptr().cast(),w!("\\VarFileInfo\\Translation"),&mut pointer,&mut length).as_bool() || length<4 {return None;}
        let translation=*(pointer as *const [u16;2]);
        let key:Vec<u16>=format!("\\StringFileInfo\\{:04x}{:04x}\\CompanyName",translation[0],translation[1]).encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(data.as_ptr().cast(),PCWSTR(key.as_ptr()),&mut pointer,&mut length).as_bool() || length==0 {return None;}
        let text=std::slice::from_raw_parts(pointer as *const u16,length as usize);
        Some(String::from_utf16_lossy(text).trim_end_matches('\0').to_string())
    })()}.unwrap_or_default();
    if let Ok(mut cache)=CACHE.lock(){cache.get_or_insert_with(Default::default).insert(path.to_string(),name.clone());}
    name
}

/// 路径完全一致；或命中「*文件名」且该程序由腾讯发布。
fn path_allowed(image:&str,paths:&[String])->bool{
    if paths.iter().any(|p|p==image){return true;}
    let file=image.rsplit('\\').next().unwrap_or(image);
    paths.iter().any(|p|p.strip_prefix('*').is_some_and(|name|name==file)) && company_name(image).to_lowercase().contains("tencent")
}

unsafe fn matches(hwnd: HWND, paths: &[String]) -> bool {
    if hwnd.0.is_null(){return false;}
    let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
    pid_matches(pid,paths)
}

unsafe fn pid_matches(pid:u32, paths: &[String]) -> bool {
    if pid==0 {return false;}
    let Ok(process)=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid) else{return false};
    let mut path=[0u16;32768];let mut length=path.len() as u32;
    let result=QueryFullProcessImageNameW(process,PROCESS_NAME_WIN32,PWSTR(path.as_mut_ptr()),&mut length);
    let _=CloseHandle(process);
    result.is_ok() && path_allowed(&normalized(&String::from_utf16_lossy(&path[..length as usize])),paths)
}

fn client_paths(source:crate::app::Messenger)->Vec<String>{
    if source==crate::app::Messenger::Wechat{return installed_paths();}
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    let mut candidates=Vec::new();
    for root in [HKEY_CURRENT_USER,HKEY_LOCAL_MACHINE]{
        for key in [w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\QQ"),w!("Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\QQ")]{
            if let Some(icon)=registry_string(root,key,w!("DisplayIcon")){
                let exe=icon.rsplit_once(',').filter(|(_,index)|index.trim().parse::<i32>().is_ok()).map_or(icon.as_str(),|(path,_)|path);
                candidates.push(PathBuf::from(exe.trim().trim_matches('"')));
            }
        }
    }
    // QQ NT 的协议处理器在版本子目录中，主程序在安装根目录。
    if let Some(command)=registry_string(HKEY_CLASSES_ROOT,w!("tencent\\shell\\open\\command"),PCWSTR::null()){
        let helper=command.trim().trim_start_matches('"').split('"').next().unwrap_or("");
        let helper=std::path::Path::new(helper);
        if helper.is_absolute() && helper.file_name().is_some_and(|n|n.eq_ignore_ascii_case("timwp.exe")){
            if let Some(versions)=helper.ancestors().find(|p|p.file_name().is_some_and(|n|n.eq_ignore_ascii_case("versions"))){
                if let Some(root)=versions.parent(){candidates.push(root.join("QQ.exe"));}
            }
        }
    }
    let mut paths:Vec<String>=candidates.into_iter().filter(|p|p.is_absolute() && p.file_name().is_some_and(|n|n.eq_ignore_ascii_case("QQ.exe")) && p.is_file())
        .filter_map(|p|p.canonicalize().ok()).map(|p|normalized(&p.to_string_lossy())).collect();
    paths.push("*qq.exe".into());
    paths
}

fn client_main_frame(source:crate::app::Messenger,class:&str,style:u32,extended:u32)->bool{
    let known=match source {
        crate::app::Messenger::Qq=>matches!(class,"Chrome_WidgetWin_1"|"TXGuiFoundation"),
        crate::app::Messenger::Wechat=>class=="WeChatMainWndForPC" || class.starts_with("Qt") && class.ends_with("QWindowIcon"),
    };
    let frame=(WS_SYSMENU|WS_MINIMIZEBOX).0;
    let resizable_frame=(WS_CAPTION|WS_THICKFRAME|WS_MINIMIZEBOX).0;
    let custom_frame=class.starts_with("Qt") || class=="Chrome_WidgetWin_1";
    known && (style & frame==frame || custom_frame && style & resizable_frame==resizable_frame) && extended & (WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE).0==0
}

fn window_pid(hwnd:HWND)->u32{
    let mut pid=0;
    if !hwnd.0.is_null(){unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}}
    pid
}

/// 前台是该客户端的主窗口时返回它所在的进程（多开时每个账号一个进程）。
unsafe fn foreground_main_pid(source:crate::app::Messenger,paths:&[String])->Option<u32>{
    let hwnd=GetForegroundWindow();
    if IsIconic(hwnd).as_bool() || !visible_main_frame(hwnd,source) || !matches(hwnd,paths){return None;}
    Some(window_pid(hwnd))
}

unsafe fn visible_main_frame(hwnd:HWND,source:crate::app::Messenger)->bool{
    if !IsWindowVisible(hwnd).as_bool() || GetWindow(hwnd,GW_OWNER).is_ok(){return false;}
    let mut class=[0u16;128];
    let n=GetClassNameW(hwnd,&mut class).max(0) as usize;
    client_main_frame(source,&String::from_utf16_lossy(&class[..n]),GetWindowLongW(hwnd,GWL_STYLE) as u32,GetWindowLongW(hwnd,GWL_EXSTYLE) as u32)
}

/// 用户真正看得到、像主窗口的窗口：排除被系统隐藏（cloaked）、不在任何显示器上、或小于主窗口的弹窗。
unsafe fn usable_main_frame(hwnd:HWND,source:crate::app::Messenger)->bool{
    use windows::Win32::{Foundation::RECT,Graphics::{Dwm::{DwmGetWindowAttribute,DWMWA_CLOAKED},Gdi::{MonitorFromWindow,MONITOR_DEFAULTTONULL}}};
    if !visible_main_frame(hwnd,source){return false;}
    let mut cloaked=0u32;
    if DwmGetWindowAttribute(hwnd,DWMWA_CLOAKED,(&mut cloaked as *mut u32).cast(),4).is_ok() && cloaked!=0 {return false;}
    if IsIconic(hwnd).as_bool(){return true;}
    let mut r=RECT::default();
    GetWindowRect(hwnd,&mut r).is_ok() && r.right-r.left>=300 && r.bottom-r.top>=300 && !MonitorFromWindow(hwnd,MONITOR_DEFAULTTONULL).0.is_null()
}

/// 进程 → 父进程。用来把闪动的子进程对应回它所属账号的微信主进程。
fn parent_processes()->std::collections::HashMap<u32,u32>{
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot,Process32FirstW,Process32NextW,PROCESSENTRY32W,TH32CS_SNAPPROCESS};
    let mut parents=std::collections::HashMap::new();
    unsafe{
        let Ok(snapshot)=CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0) else {return parents};
        let mut entry=PROCESSENTRY32W{dwSize:std::mem::size_of::<PROCESSENTRY32W>() as u32,..Default::default()};
        let mut ok=Process32FirstW(snapshot,&mut entry).is_ok();
        while ok {parents.insert(entry.th32ProcessID,entry.th32ParentProcessID);ok=Process32NextW(snapshot,&mut entry).is_ok();}
        let _=CloseHandle(snapshot);
    }
    parents
}

/// 从 `pid` 往上的祖先进程（不含自己），最多 8 层，遇到环或系统进程即止。
fn ancestors(pid:u32,parents:&std::collections::HashMap<u32,u32>)->Vec<u32>{
    let mut chain=Vec::new();let mut current=pid;
    for _ in 0..8 {
        match parents.get(&current){Some(&parent) if parent!=0 && parent!=current && parent!=pid && !chain.contains(&parent)=>{chain.push(parent);current=parent;},_=>break}
    }
    chain
}

pub fn client_in_foreground(source:crate::app::Messenger)->bool{
    unsafe{foreground_main_pid(source,&client_paths(source)).is_some()}
}

/// 当前前台的微信 / QQ 主窗口：来源与账号进程。
pub fn foreground_client(sources:&[crate::app::Messenger])->Option<(crate::app::Messenger,u32)>{
    sources.iter().find_map(|&source|unsafe{foreground_main_pid(source,&client_paths(source))}.map(|pid|(source,pid)))
}

/// 跳到某条未读对应的窗口。
pub fn open_unread(unread:crate::app::Unread)->Result<(),String>{
    open_unread_with_paths(unread,&client_paths(unread.source))
}

fn open_unread_with_paths(unread:crate::app::Unread,paths:&[String])->Result<(),String>{
    let window=HWND(unread.window as *mut _);
    if unread.source==crate::app::Messenger::Wechat {
        // 微信 4 闪动的句柄常是隐藏的 Qt 辅助窗口：从外部显示它会出现白屏、拖死微信。
        // 只切换到该账号进程里已可见的主窗口；绝不显示隐藏窗口，也不另起 Weixin.exe。
        unsafe {
            let target=if unread.pid==0 {None}
                else if window_pid(window)==unread.pid && usable_main_frame(window,unread.source) && matches(window,paths) {Some(window)}
                else {account_main_window(unread.pid,unread.source,paths)};
            if let Some(target)=target {
                // 异步恢复：微信卡住时也不能拖住岛仔的界面线程。
                if IsIconic(target).as_bool(){ShowWindowAsync(target,SW_RESTORE).ok().map_err(|_|"请从任务栏打开微信".to_string())?;}
                if SetForegroundWindow(target).as_bool(){return Ok(());}
            }
        }
        return Err("请从任务栏或托盘打开微信".into());
    }
    unsafe{
        if unread.window!=0 && IsWindow(window).as_bool() && window_pid(window)==unread.pid && matches(window,paths){
            if IsIconic(window).as_bool(){let _=ShowWindowAsync(window,SW_RESTORE);}
            else if !IsWindowVisible(window).as_bool(){let _=ShowWindowAsync(window,SW_SHOW);}
            if SetForegroundWindow(window).as_bool(){return Ok(());}
        }
    }
    open_client(unread.source)
}

/// 托盘唤起的诊断：只记步骤、窗口类名、位置和系统版本，不记窗口标题、提示文字或聊天内容。
struct TrayLog(Vec<String>);
impl TrayLog {
    fn new(unread:crate::app::Unread)->Self{
        use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
        let key=w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
        let build=registry_string(HKEY_LOCAL_MACHINE,key,w!("CurrentBuild")).unwrap_or_default();
        let display=registry_string(HKEY_LOCAL_MACHINE,key,w!("DisplayVersion")).unwrap_or_default();
        Self(vec![format!("岛仔 {} · Windows 内部版本 {build} ({display}) · 账号进程 {} · 闪动窗口 {:#x}",env!("CARGO_PKG_VERSION"),unread.pid,unread.window)])
    }
    fn step(&mut self,line:impl Into<String>){self.0.push(line.into());}
    /// 写入日志并返回给用户看的提示（带错误码，便于反馈）。
    fn fail(mut self,code:&str,reason:&str)->String{
        self.step(format!("失败 {code}：{reason}"));
        let directory=crate::model::config_dir().join("diagnostics");
        let saved=std::fs::create_dir_all(&directory).and_then(|_|std::fs::write(directory.join("wechat-tray.log"),self.0.join("\n")+"\n")).is_ok();
        if saved {format!("未能唤起微信（{code}），请从托盘打开；诊断已记录")} else {format!("未能唤起微信（{code}），请从托盘打开")}
    }
}

fn class_name(hwnd:HWND)->String{
    let mut class=[0u16;128];
    let n=unsafe{GetClassNameW(hwnd,&mut class)}.max(0) as usize;
    String::from_utf16_lossy(&class[..n])
}

/// 所有微信进程的托盘消息窗口：（进程，窗口）。
unsafe fn tray_hosts(paths:&[String])->Vec<(u32,HWND)>{
    use windows::Win32::Foundation::BOOL;
    struct Find<'a>{paths:&'a [String],hosts:Vec<(u32,HWND)>}
    unsafe extern "system" fn visit(hwnd:HWND,data:LPARAM)->BOOL{
        let find=&mut *(data.0 as *mut Find);
        // 微信 4.x 为 Qt…WxTrayIconMessageWindowClass；只认类名里带 TrayIcon 且属于微信安装路径的窗口。
        if class_name(hwnd).contains("TrayIcon") && matches(hwnd,find.paths){find.hosts.push((window_pid(hwnd),hwnd));}
        true.into()
    }
    let mut find=Find{paths,hosts:Vec::new()};
    let _=EnumWindows(Some(visit),LPARAM((&mut find as *mut Find) as isize));
    find.hosts
}

/// 该账号的托盘窗口与唯一的图标编号。闪动进程没有托盘窗口而全机只有一个微信托盘时（单开），用那一个。
unsafe fn tray_icon(pid:u32,paths:&[String],log:&mut TrayLog)->Option<(HWND,u32)>{
    let hosts=tray_hosts(paths);
    log.step(format!("微信托盘窗口：{}",hosts.iter().map(|(p,h)|format!("进程 {p} {} {:#x}",class_name(*h),h.0 as usize)).collect::<Vec<_>>().join("；")));
    let parents=parent_processes();
    let mut host_pids:Vec<u32>=hosts.iter().map(|(p,_)|*p).collect();host_pids.sort_unstable();host_pids.dedup();
    log.step(format!("闪动进程 {pid} 的祖先：{:?}",ancestors(pid,&parents)));
    let account=account_tray_pid(pid,&host_pids,&parents).or_else(||{
        // 只有闪动来自微信进程（或来源未知）时，才退到全机唯一的微信托盘（单开）。
        match host_pids.as_slice(){[only] if pid==0 || pid_matches(pid,paths)=>Some(*only),_=>None}
    });
    log.step(format!("对应的托盘进程：{account:?}"));
    let candidates:Vec<HWND>=hosts.iter().filter(|(p,_)|Some(*p)==account).map(|(_,h)|*h).collect();
    // 同一进程可能有多个托盘窗口：全部图标里只有一个时才用，否则分不清账号就放弃。
    let icons:Vec<(HWND,u32)>=candidates.iter().flat_map(|&h|(0..=256).filter(move |id|tray_rect(h,*id).is_some()).map(move |id|(h,id))).collect();
    log.step(format!("托盘图标：{}",icons.iter().map(|(h,id)|format!("{:#x}#{id}",h.0 as usize)).collect::<Vec<_>>().join("，")));
    match icons.as_slice(){[one]=>Some(*one),_=>None}
}

/// 闪动进程所属账号的托盘进程：自己 → 最近的祖先 → 唯一的后代。双开时两个账号互不相干，不会串号。
fn account_tray_pid(pid:u32,hosts:&[u32],parents:&std::collections::HashMap<u32,u32>)->Option<u32>{
    if pid==0 {return None;}
    if hosts.contains(&pid){return Some(pid);}
    if let Some(ancestor)=ancestors(pid,parents).into_iter().find(|a|hosts.contains(a)){return Some(ancestor);}
    let below:Vec<u32>=hosts.iter().copied().filter(|h|ancestors(*h,parents).contains(&pid)).collect();
    match below.as_slice(){[one]=>Some(*one),_=>None}
}

/// 该账号能切换过去的主窗口：先找闪动进程自己，再找它的微信祖先进程。
unsafe fn account_main_window(pid:u32,source:crate::app::Messenger,paths:&[String])->Option<HWND>{
    if pid==0 {return None;}
    let parents=parent_processes();
    std::iter::once(pid).chain(ancestors(pid,&parents).into_iter().filter(|p|pid_matches(*p,paths)))
        .find_map(|p|visible_main_window_of(p,source,paths))
}

/// 该账号缩在托盘（没有可见主窗口，但有托盘图标）时返回 Ok；否则返回给用户的提示，并记录诊断。
pub fn wechat_in_tray(unread:crate::app::Unread)->Result<(),String>{
    if unread.source!=crate::app::Messenger::Wechat {return Err("请从任务栏或托盘打开微信".into());}
    let paths=client_paths(unread.source);
    let mut log=TrayLog::new(unread);
    unsafe{
        let Some((host,_))=tray_icon(unread.pid,&paths,&mut log) else {return Err(log.fail("T1","未找到该账号唯一的微信托盘图标"));};
        let pid=window_pid(host);
        if let Some(main)=visible_main_window_of(pid,unread.source,&paths).or_else(||account_main_window(unread.pid,unread.source,&paths)){
            return Err(log.fail("T0",&format!("微信主窗口仍可见（{} {:#x}），但未能切到前台",class_name(main),main.0 as usize)));
        }
    }
    Ok(())
}

unsafe fn tray_rect(host:HWND,id:u32)->Option<windows::Win32::Foundation::RECT>{
    use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect,NOTIFYICONIDENTIFIER};
    let icon=NOTIFYICONIDENTIFIER{cbSize:std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,hWnd:host,uID:id,..Default::default()};
    Shell_NotifyIconGetRect(&icon).ok()
}

fn rect_center_inside(inner:windows::Win32::Foundation::RECT,outer:windows::Win32::Foundation::RECT)->bool{
    let (x,y)=((inner.left+inner.right)/2,(inner.top+inner.bottom)/2);
    outer.right>outer.left && outer.bottom>outer.top && x>=outer.left && x<outer.right && y>=outer.top && y<outer.bottom
}

/// 覆盖图标中心、大小与托盘按钮相当的控件（排除整条任务栏之类的容器）。
fn button_sized(icon:windows::Win32::Foundation::RECT,button:windows::Win32::Foundation::RECT)->bool{
    let (iw,ih)=((icon.right-icon.left).max(16),(icon.bottom-icon.top).max(16));
    rect_center_inside(icon,button) && button.right-button.left<=iw*3 && button.bottom-button.top<=ih*3
}

fn show_rect(r:windows::Win32::Foundation::RECT)->String{format!("({},{})-({},{})",r.left,r.top,r.right,r.bottom)}

/// 微信缩在托盘时：通过无障碍接口按下该账号自己的托盘按钮，由微信自己唤起主窗口。
/// 不移动鼠标、不向微信发送内部消息、不注入、不读取内容；定位不到唯一按钮就放弃。
/// 兼容新版 Win11 托盘与 Windows 10 / 早期 Win11 的旧式托盘。会阻塞几百毫秒，必须在后台线程调用。
pub fn open_wechat_from_tray(unread:crate::app::Unread)->Result<(),String>{
    use windows::{core::Interface,Win32::{Foundation::RECT,System::Com::*,UI::{Accessibility::*,HiDpi::*}}};
    let mut log=TrayLog::new(unread);
    if unread.source!=crate::app::Messenger::Wechat {return Err(log.fail("T1","不是微信提醒"));}
    let paths=client_paths(unread.source);
    unsafe {
        let _=SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _=CoInitializeEx(None,COINIT_MULTITHREADED);
        let Some((host,id))=tray_icon(unread.pid,&paths,&mut log) else {return Err(log.fail("T1","未找到该账号唯一的微信托盘图标"));};
        let Ok(ui)=CoCreateInstance::<_,IUIAutomation>(&CUIAutomation8,None,CLSCTX_INPROC_SERVER) else {return Err(log.fail("T2","无法创建系统无障碍接口"));};
        if let Ok(timed)=ui.cast::<IUIAutomation2>(){let _=timed.SetConnectionTimeout(800);let _=timed.SetTransactionTimeout(800);}
        let Ok(all)=ui.CreateTrueCondition() else {return Err(log.fail("T2","无障碍查询条件创建失败"));};
        // 在某个窗口里找覆盖图标中心、大小像托盘按钮、可以按下的最小控件。
        let button_at=|window:HWND,icon:RECT,log:&mut TrayLog|->Option<IUIAutomationElement>{
            let root=ui.ElementFromHandle(window).ok()?;
            let items=root.FindAll(TreeScope_Descendants,&all).ok()?;
            let mut best:Option<(i64,IUIAutomationElement)>=None;
            for i in 0..items.Length().ok()? {
                let Ok(item)=items.GetElement(i) else {continue};
                let Ok(r)=item.CurrentBoundingRectangle() else {continue};
                if !button_sized(icon,r) || item.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId).is_err() {continue;}
                let area=i64::from(r.right-r.left)*i64::from(r.bottom-r.top);
                if best.as_ref().is_none_or(|(old,_)|area<*old){best=Some((area,item));}
            }
            if let Some((_,item))=&best {
                log.step(format!("命中按钮：{} · {} · {}",item.CurrentAutomationId().map(|s|s.to_string()).unwrap_or_default(),item.CurrentClassName().map(|s|s.to_string()).unwrap_or_default(),item.CurrentBoundingRectangle().map(show_rect).unwrap_or_default()));
            }
            best.map(|(_,item)|item)
        };
        let invoke=|element:&IUIAutomationElement|->bool{
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId).and_then(|p|p.Invoke()).is_ok()
        };
        // 新版 Win11 与旧式（Windows 10 / 早期 Win11）的折叠区窗口。
        let overflow_open=||[w!("TopLevelWindowForOverflowXamlIsland"),w!("NotifyIconOverflowWindow")].into_iter()
            .find_map(|class|FindWindowW(class,PCWSTR::null()).ok().filter(|h|IsWindowVisible(*h).as_bool()));
        let Ok(taskbar)=FindWindowW(w!("Shell_TrayWnd"),PCWSTR::null()) else {return Err(log.fail("T3","找不到任务栏"));};
        let Some(icon)=tray_rect(host,id) else {return Err(log.fail("T1","系统未报告托盘图标位置"));};
        log.step(format!("图标位置：{}",show_rect(icon)));
        let was_open=overflow_open().is_some();
        let chevron:IUIAutomationElement;
        let from_chevron:bool;
        if let Some(button)=button_at(taskbar,icon,&mut log) {
            if !invoke(&button){return Err(log.fail("T5","任务栏按钮无法按下"));}
            std::thread::sleep(Duration::from_millis(250));
            // 按下后折叠区展开了：说明图标收在折叠区，命中的是「显示隐藏的图标」（新版 Win11 会报告这个位置）。
            if was_open || overflow_open().is_none() {return Ok(());}
            log.step("命中的是折叠按钮，继续在折叠区里找");
            chevron=button;from_chevron=true;
        } else {
            // 旧式托盘：收起的图标位置在隐藏的折叠窗口里，先用「显示隐藏的图标」按钮展开。
            let tray=FindWindowExW(taskbar,None,w!("TrayNotifyWnd"),PCWSTR::null()).ok();
            let button=tray.and_then(|t|FindWindowExW(t,None,w!("Button"),PCWSTR::null()).ok());
            log.step(format!("任务栏上未命中；旧式折叠按钮：{}",button.map(|b|format!("{:#x}",b.0 as usize)).unwrap_or("无".into())));
            let Some(element)=button.and_then(|b|ui.ElementFromHandle(b).ok()) else {return Err(log.fail("T3","任务栏上找不到微信托盘按钮"));};
            if !was_open && !invoke(&element){return Err(log.fail("T5","折叠按钮无法按下"));}
            chevron=element;from_chevron=false;
        }
        let started=Instant::now();
        let mut pressed=false;
        while started.elapsed()<Duration::from_millis(1500) {
            std::thread::sleep(Duration::from_millis(80));
            let Some(overflow)=overflow_open() else {continue};
            // 折叠区展开后，系统报告的才是图标在折叠区里的真实位置；多开时据此区分账号。
            let Some(inside)=tray_rect(host,id).filter(|r|!from_chevron || !rect_center_inside(*r,icon)) else {continue};
            if let Some(button)=button_at(overflow,inside,&mut log){
                log.step(format!("折叠区 {} 图标位置：{}",class_name(overflow),show_rect(inside)));
                pressed=invoke(&button);
                break;
            }
        }
        if pressed {return Ok(());}
        // 没按成：把展开的折叠区收回去，不留在屏幕上。
        if !was_open && overflow_open().is_some(){let _=invoke(&chevron);}
        Err(log.fail(if started.elapsed()>=Duration::from_millis(1500){"T4"}else{"T5"},"折叠区里找不到或按不下微信托盘按钮"))
    }
}

/// 某个账号进程里用户看得到的主窗口（多开时每个进程一个）：有多个时取最大的，最小化的排在后面。
unsafe fn visible_main_window_of(pid:u32,source:crate::app::Messenger,paths:&[String])->Option<HWND>{
    use windows::Win32::Foundation::{BOOL,RECT};
    struct Find<'a>{pid:u32,source:crate::app::Messenger,paths:&'a [String],best:Option<(i64,HWND)>}
    unsafe extern "system" fn visit(hwnd:HWND,data:LPARAM)->BOOL{
        let find=&mut *(data.0 as *mut Find);
        if window_pid(hwnd)==find.pid && usable_main_frame(hwnd,find.source) && matches(hwnd,find.paths){
            let mut r=RECT::default();let _=GetWindowRect(hwnd,&mut r);
            let area=if IsIconic(hwnd).as_bool(){0}else{i64::from(r.right-r.left)*i64::from(r.bottom-r.top)};
            if find.best.is_none_or(|(old,_)|area>old){find.best=Some((area,hwnd));}
        }
        true.into()
    }
    let mut find=Find{pid,source,paths,best:None};
    let _=EnumWindows(Some(visit),LPARAM((&mut find as *mut Find) as isize));
    find.best.map(|(_,hwnd)|hwnd)
}

pub fn open_client(source:crate::app::Messenger)->Result<(),String>{
    use windows::Win32::Foundation::{BOOL,RECT};
    struct Search {paths:Vec<String>,best:Option<(i64,HWND)>,tray:Vec<HWND>,qq:bool}
    unsafe extern "system" fn visit(hwnd:HWND,data:LPARAM)->BOOL{
        let search=&mut *(data.0 as *mut Search);
        if search.qq{
            let mut class=[0u16;64];
            let count=GetClassNameW(hwnd,&mut class).max(0) as usize;
            let class=String::from_utf16_lossy(&class[..count]);
            if class=="Electron_NotifyIconHostWindow" && matches(hwnd,&search.paths){search.tray.push(hwnd);}
            if !matches!(class.as_str(),"Chrome_WidgetWin_1"|"TXGuiFoundation"){return true.into();}
        }
        if IsWindowVisible(hwnd).as_bool() && GetWindow(hwnd,GW_OWNER).is_err() && matches(hwnd,&search.paths){
            let mut r=RECT::default();
            if GetWindowRect(hwnd,&mut r).is_ok(){
                let area=i64::from(r.right-r.left)*i64::from(r.bottom-r.top);
                if area>0 && search.best.is_none_or(|(old,_)|area>old){search.best=Some((area,hwnd));}
            }
        }
        true.into()
    }
    let mut search=Search{paths:client_paths(source),best:None,tray:Vec::new(),qq:source==crate::app::Messenger::Qq};
    let exe=search.paths.iter().find(|p|!p.starts_with('*')).ok_or_else(||format!("未找到{}的安装路径",source.name()))?.clone();
    unsafe{
        let _=EnumWindows(Some(visit),LPARAM((&mut search as *mut Search) as isize));
        if let Some((_,window))=search.best{
            if IsIconic(window).as_bool(){let _=ShowWindowAsync(window,SW_RESTORE);}
            if SetForegroundWindow(window).as_bool(){return Ok(());}
        }
        if let [host]=search.tray.as_slice(){
            use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect,NOTIFYICONIDENTIFIER};
            let mut found=None;
            for id in 2..=256 {
                let icon=NOTIFYICONIDENTIFIER{cbSize:std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,hWnd:*host,uID:id,..Default::default()};
                if Shell_NotifyIconGetRect(&icon).is_ok(){
                    if found.is_some(){found=None;break;} found=Some(id);
                }
            }
            if let Some(id)=found {
                for event in [WM_LBUTTONDOWN,WM_LBUTTONUP,WM_LBUTTONDBLCLK,WM_LBUTTONUP]{
                    if !PostMessageW(*host,WM_APP+1,WPARAM(id as usize),LPARAM(event as isize)).is_ok(){return Err("QQ 托盘暂时无法响应，请重试".into());}
                }
                return Ok(());
            }
        }
    }
    std::process::Command::new(exe).spawn().map(|_|()).map_err(|e|format!("无法打开{}：{e}",source.name()))
}

struct State {
    message: u32,
    taskbar_created: u32,
    registered: bool,
    paths: Vec<String>,
    /// 每个微信进程（账号）各自合并连续闪动。
    burst: Vec<(u32,Burst)>,
    /// 尚未取走的提醒：账号进程与闪动的窗口。
    pending: Vec<(u32,isize)>,
    /// 当前正在前台查看的账号进程。
    viewing: Vec<u32>,
    ctx: eframe::egui::Context,
}

unsafe extern "system" fn subclass(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
    {
        let slot=&*(data as *const RefCell<State>);
        if let Ok(mut state)=slot.try_borrow_mut(){
            if message==state.taskbar_created && message!=0 {
                state.registered=RegisterShellHookWindow(hwnd).as_bool();
                state.ctx.request_repaint();
            }
            if message==state.message && matches!(wp.0, FLASH | 4 | 0x8004) {
                let target=HWND(lp.0 as *mut _);
                let foreground=foreground_main_pid(crate::app::Messenger::Wechat,&state.paths);
                // 刚看过的账号已不在前台（Alt+Tab 切走、点了别的窗口，或漏掉了切换事件）：开始宽限期。
                let now=Instant::now();
                let left:Vec<u32>=state.viewing.iter().copied().filter(|p|Some(*p)!=foreground).collect();
                for pid in left {
                    trace(format!("切走 账号 {pid}：开始 {}s 宽限期",LEAVE_GRACE.as_secs()));
                    state.viewing.retain(|p|*p!=pid);
                    state.burst.retain(|(p,_)|*p!=pid);state.burst.push((pid,Burst::left(now)));
                }
                let ours=matches(target,&state.paths);
                if ours {
                    trace(format!("{} 目标 {}·进程 {}·{} 前台 {}·微信主窗口进程 {foreground:?}",match wp.0{FLASH=>"闪动",4=>"激活",_=>"激活(rude)"},describe(target),window_pid(target),if ours{"微信"}else{"非微信"},describe(GetForegroundWindow())));
                }
                if ours{
                    let pid=window_pid(target);
                    if foreground==Some(pid){
                        if state.pending.iter().any(|(p,_)|*p==pid){trace(format!("  → 账号 {pid} 在前台，清掉待取提醒"));}
                        // 正在看这个账号：它的提醒作废，其它账号不受影响。
                        state.burst.retain(|(p,_)|*p!=pid);state.pending.retain(|(p,_)|*p!=pid);
                        if !state.viewing.contains(&pid){state.viewing.push(pid);}
                    }else if wp.0==FLASH {
                        let fresh={
                            if !state.burst.iter().any(|(p,_)|*p==pid){state.burst.push((pid,Burst::default()));}
                            state.burst.iter_mut().find(|(p,_)|*p==pid).is_some_and(|(_,burst)|burst.attention(Instant::now()))
                        };
                        trace(format!("  → {}",if fresh{"新提醒"}else{"合并到上一轮 / 宽限期内，不提醒"}));
                        if fresh {
                            match state.pending.iter_mut().find(|(p,_)|*p==pid){Some(entry)=>entry.1=target.0 as isize,None=>state.pending.push((pid,target.0 as isize))}
                            state.ctx.request_repaint();
                        }
                    }
                }
            }
        }
    }
    if message==WM_NCDESTROY {
        let _=DeregisterShellHookWindow(hwnd);
        let _=RemoveWindowSubclass(hwnd,Some(subclass),SUBCLASS_ID);
    }
    DefSubclassProc(hwnd,message,wp,lp)
}

/// 窗口的简短描述：句柄、类名、可见 / 最小化。不读标题。
fn describe(hwnd:HWND)->String{
    if hwnd.0.is_null(){return "无".into();}
    unsafe{format!("{:#x} {} {}{}",hwnd.0 as usize,class_name(hwnd),if IsWindowVisible(hwnd).as_bool(){"可见"}else{"隐藏"},if IsIconic(hwnd).as_bool(){"·最小化"}else{""})}
}

/// 当前前台窗口的描述（给主程序写事件记录用）。
pub fn describe_foreground()->String{unsafe{let h=GetForegroundWindow();format!("{} 进程 {}",describe(h),window_pid(h))}}

/// 微信提醒事件记录：只保留最近 300 条，写到 diagnostics/wechat-events.log；不记标题和聊天内容。
pub fn trace(line:impl Into<String>){
    static LOG:std::sync::Mutex<std::collections::VecDeque<String>>=std::sync::Mutex::new(std::collections::VecDeque::new());
    let Ok(mut log)=LOG.lock() else {return};
    log.push_back(format!("{} {}",chrono::Local::now().format("%H:%M:%S%.3f"),line.into()));
    while log.len()>300 {log.pop_front();}
    let directory=crate::model::config_dir().join("diagnostics");
    if std::fs::create_dir_all(&directory).is_ok(){let _=std::fs::write(directory.join("wechat-events.log"),log.iter().cloned().collect::<Vec<_>>().join("\n")+"\n");}
}

pub struct Listener { hwnd: HWND, state: Box<RefCell<State>> }
impl Listener {
    pub fn new(ctx: eframe::egui::Context) -> Result<Self,String> {
        let paths=installed_paths();
        if paths.is_empty(){return Err("未找到微信安装路径，请安装微信后重新开启提醒".into());}
        unsafe {
            let message=RegisterWindowMessageW(w!("SHELLHOOK"));
            if message==0{return Err("无法注册微信提醒事件".into());}
            let hwnd=CreateWindowExW(WS_EX_TOOLWINDOW,w!("STATIC"),w!("DaozaiWechatListener"),WS_POPUP,0,0,0,0,None,None,None,None).map_err(|_|"无法创建微信监听窗口")?;
            let state=Box::new(RefCell::new(State{message,taskbar_created:RegisterWindowMessageW(w!("TaskbarCreated")),registered:true,paths,burst:Vec::new(),pending:Vec::new(),viewing:Vec::new(),ctx}));
            let listener=Self{hwnd,state};
            SetWindowSubclass(hwnd,Some(subclass),SUBCLASS_ID,(&*listener.state as *const RefCell<State>) as usize).ok().map_err(|_|"无法接入微信提醒事件")?;
            RegisterShellHookWindow(hwnd).ok().map_err(|_|"无法监听微信任务栏提醒")?;
            Ok(listener)
        }
    }
    pub fn registered(&self)->bool{self.state.borrow().registered}
    /// 取走新提醒：每项是（账号进程，闪动的窗口）。
    pub fn take(&self) -> Vec<(u32,isize)> {std::mem::take(&mut self.state.borrow_mut().pending)}
}
impl Drop for Listener {
    fn drop(&mut self){unsafe{
        let _=DeregisterShellHookWindow(self.hwnd);
        let _=RemoveWindowSubclass(self.hwnd,Some(subclass),SUBCLASS_ID);
        let _=DestroyWindow(self.hwnd);
    }}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clicking_wechat_flash_never_shows_hidden_helper(){unsafe{
        let window=CreateWindowExW(WS_EX_TOOLWINDOW,w!("STATIC"),w!(""),WS_POPUP,0,0,0,0,None,None,None,None).unwrap();
        let paths=[normalized(&std::env::current_exe().unwrap().to_string_lossy())];
        let unread=crate::app::Unread{source:crate::app::Messenger::Wechat,pid:std::process::id(),window:window.0 as isize};
        let result=open_unread_with_paths(unread,&paths);
        let visible=IsWindowVisible(window).as_bool();
        DestroyWindow(window).unwrap();
        assert!(!visible && result.is_err(),"点击提醒不应显示隐藏的辅助窗口");
    }}
    #[test]
    fn clicking_wechat_flash_never_shows_hidden_qt_frame(){unsafe{
        // 复现 v0.6.60 白屏：微信 4 隐藏的 Qt 窗口带系统菜单 / 最小化按钮，过去会被 SW_SHOW 显示出来。
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        unsafe extern "system" fn proc(h:HWND,m:u32,w:WPARAM,l:LPARAM)->LRESULT{DefWindowProcW(h,m,w,l)}
        let instance=GetModuleHandleW(None).unwrap().into();
        let class=WNDCLASSW{lpfnWndProc:Some(proc),hInstance:instance,lpszClassName:w!("Qt51514QWindowIcon"),..Default::default()};
        RegisterClassW(&class);
        let style=WINDOW_STYLE(0xC60B0000 & !WS_CHILD.0);
        let window=CreateWindowExW(WINDOW_EX_STYLE(0),w!("Qt51514QWindowIcon"),w!("Weixin"),style,0,0,300,200,None,None,instance,None).unwrap();
        let paths=[normalized(&std::env::current_exe().unwrap().to_string_lossy())];
        let pid=std::process::id();
        let flashed=open_unread_with_paths(crate::app::Unread{source:crate::app::Messenger::Wechat,pid,window:window.0 as isize},&paths);
        let unknown=open_unread_with_paths(crate::app::Unread{source:crate::app::Messenger::Wechat,pid:0,window:window.0 as isize},&paths);
        let visible=IsWindowVisible(window).as_bool();
        DestroyWindow(window).unwrap();
        assert!(!visible,"隐藏的微信窗口不得被显示");
        assert!(flashed.is_err() && unknown.is_err());
    }}
    #[test]
    #[ignore="需要本机已启动的微信；会把微信主窗口切到前台"]
    fn local_wechat_hidden_windows_stay_hidden_when_opened(){unsafe{
        use windows::Win32::Foundation::BOOL;
        unsafe extern "system" fn collect(hwnd:HWND,data:LPARAM)->BOOL{
            let (paths,out)=&mut *(data.0 as *mut (Vec<String>,Vec<(HWND,bool)>));
            if matches(hwnd,paths){out.push((hwnd,IsWindowVisible(hwnd).as_bool()));}
            true.into()
        }
        let mut found=(installed_paths(),Vec::new());
        let _=EnumWindows(Some(collect),LPARAM((&mut found as *mut (Vec<String>,Vec<(HWND,bool)>)) as isize));
        assert!(!found.1.is_empty(),"本机没有微信窗口");
        for &(hwnd,_) in &found.1 {
            let unread=crate::app::Unread{source:crate::app::Messenger::Wechat,pid:window_pid(hwnd),window:hwnd.0 as isize};
            let result=open_unread_with_paths(unread,&found.0);
            std::thread::sleep(Duration::from_millis(300));
            for &(other,was_visible) in &found.1 {
                assert!(was_visible || !IsWindowVisible(other).as_bool(),"隐藏窗口 {other:?} 被显示（目标 {hwnd:?}，结果 {result:?}）");
            }
            println!("目标 {hwnd:?} 可见={} → {result:?}",found.1.iter().any(|(h,v)|*h==hwnd && *v));
        }
    }}
    #[test]
    fn dual_open_flashes_map_to_their_own_tray(){
        use std::collections::HashMap;
        // 两个账号：主号 100（子进程 101、102），备用号 200（子进程 201），都由启动器 50 拉起。
        let parents:HashMap<u32,u32>=[(100,50),(101,100),(102,101),(200,50),(201,200),(50,4)].into_iter().collect();
        let hosts=[100,200];
        assert_eq!(account_tray_pid(100,&hosts,&parents),Some(100));
        assert_eq!(account_tray_pid(102,&hosts,&parents),Some(100),"主号子进程的闪动对应主号托盘");
        assert_eq!(account_tray_pid(201,&hosts,&parents),Some(200),"备用号子进程的闪动对应备用号托盘");
        assert_eq!(account_tray_pid(50,&hosts,&parents),None,"启动器下有两个账号时分不清，不得猜");
        assert_eq!(account_tray_pid(0,&hosts,&parents),None);
        // 备用号由主号拉起：备用号子进程取最近的祖先，不串到主号。
        let nested:HashMap<u32,u32>=[(100,50),(200,100),(201,200)].into_iter().collect();
        assert_eq!(account_tray_pid(201,&hosts,&nested),Some(200));
        // 托盘在闪动进程的子进程里（只有一个时才用）。
        assert_eq!(account_tray_pid(100,&[300],&[(300,100)].into_iter().collect()),Some(300));
        // 父进程表里有环也不会卡住。
        let cycle:HashMap<u32,u32>=[(1,2),(2,1)].into_iter().collect();
        assert_eq!(account_tray_pid(1,&[9],&cycle),None);
    }
    #[test]
    fn offscreen_wechat_frame_is_not_treated_as_open(){unsafe{
        // 复现双开主号：有一个「可见」但不在任何显示器上的微信窗口，过去会被当成主窗口「切换」过去，用户什么也看不到。
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        unsafe extern "system" fn proc(h:HWND,m:u32,w:WPARAM,l:LPARAM)->LRESULT{DefWindowProcW(h,m,w,l)}
        let instance=GetModuleHandleW(None).unwrap().into();
        let class=WNDCLASSW{lpfnWndProc:Some(proc),hInstance:instance,lpszClassName:w!("Qt51514QWindowIcon"),..Default::default()};
        RegisterClassW(&class);
        let style=WS_POPUP|WS_SYSMENU|WS_MINIMIZEBOX|WS_CAPTION;
        let window=CreateWindowExW(WINDOW_EX_STYLE(0),w!("Qt51514QWindowIcon"),w!("Weixin"),style,-30000,-30000,900,700,None,None,instance,None).unwrap();
        let _=ShowWindow(window,SW_SHOWNOACTIVATE);
        let paths=[normalized(&std::env::current_exe().unwrap().to_string_lossy())];
        let pid=std::process::id();
        let qualifies=visible_main_frame(window,crate::app::Messenger::Wechat);
        let usable=usable_main_frame(window,crate::app::Messenger::Wechat);
        let found=visible_main_window_of(pid,crate::app::Messenger::Wechat,&paths);
        let result=open_unread_with_paths(crate::app::Unread{source:crate::app::Messenger::Wechat,pid,window:window.0 as isize},&paths);
        DestroyWindow(window).unwrap();
        assert!(qualifies,"测试窗口应当像微信主窗口");
        assert!(!usable && found.is_none(),"屏幕外的窗口不算用户看得到的主窗口");
        assert!(result.is_err(),"不得假装切换成功");
    }}
    #[test]
    fn process_tree_is_read_from_the_system(){
        let parents=parent_processes();
        assert!(parents.contains_key(&std::process::id()));
        assert!(!ancestors(std::process::id(),&parents).is_empty(),"测试进程应有父进程");
    }
    #[test]
    fn tray_buttons_match_by_icon_center(){
        use windows::Win32::Foundation::RECT;
        let icon=RECT{left:1531,top:1017,right:1581,bottom:1067};
        assert!(rect_center_inside(icon,RECT{left:1532,top:1018,right:1582,bottom:1068}));
        assert!(!rect_center_inside(icon,RECT{left:1582,top:1018,right:1632,bottom:1068}),"相邻图标不得命中");
        assert!(!rect_center_inside(icon,RECT::default()),"空矩形不得命中");
        // 旧式托盘按钮比图标略大；整条任务栏这类容器不算按钮。
        assert!(button_sized(RECT{left:1600,top:1050,right:1616,bottom:1066},RECT{left:1596,top:1044,right:1620,bottom:1074}));
        assert!(!button_sized(icon,RECT{left:0,top:1140,right:1920,bottom:1200}));
    }
    #[test]
    fn tray_open_rejects_unknown_accounts(){
        // 非微信进程的闪动不得退到本机微信托盘（本机装了微信时也一样）。
        assert!(open_wechat_from_tray(crate::app::Unread{source:crate::app::Messenger::Wechat,pid:std::process::id(),window:0}).is_err());
        assert!(wechat_in_tray(crate::app::Unread{source:crate::app::Messenger::Wechat,pid:std::process::id(),window:0}).is_err());
        assert!(open_wechat_from_tray(crate::app::Unread{source:crate::app::Messenger::Qq,pid:1,window:0}).is_err());
        assert!(wechat_in_tray(crate::app::Unread{source:crate::app::Messenger::Qq,pid:std::process::id(),window:0}).is_err(),"QQ 不走微信托盘");
    }
    #[test]
    #[ignore="需要本机已登录的微信；会把微信关到托盘再唤起"]
    fn local_wechat_opens_from_tray(){unsafe{
        let paths=installed_paths();
        let pid=std::process::id();let _=pid;
        let main=[0u32].into_iter().find_map(|_|{
            use windows::Win32::Foundation::BOOL;
            unsafe extern "system" fn visit(hwnd:HWND,data:LPARAM)->BOOL{
                let (paths,out)=&mut *(data.0 as *mut (Vec<String>,Option<HWND>));
                if visible_main_frame(hwnd,crate::app::Messenger::Wechat) && matches(hwnd,paths){*out=Some(hwnd);return false.into();}
                true.into()
            }
            let mut found=(paths.clone(),None);
            let _=EnumWindows(Some(visit),LPARAM((&mut found as *mut (Vec<String>,Option<HWND>)) as isize));
            found.1
        }).expect("先打开微信主窗口");
        let unread=crate::app::Unread{source:crate::app::Messenger::Wechat,pid:window_pid(main),window:main.0 as isize};
        PostMessageW(main,WM_SYSCOMMAND,WPARAM(SC_CLOSE as usize),LPARAM(0)).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert!(!IsWindowVisible(main).as_bool(),"微信应已缩到托盘");
        assert!(wechat_in_tray(unread).is_ok());
        assert!(open_unread_with_paths(unread,&paths).is_err(),"缩在托盘时普通跳转不碰任何窗口");
        let result=open_wechat_from_tray(unread);
        std::thread::sleep(Duration::from_secs(2));
        let overflow=FindWindowW(w!("TopLevelWindowForOverflowXamlIsland"),PCWSTR::null()).ok().is_some_and(|h|IsWindowVisible(h).as_bool());
        println!("托盘唤起 → {result:?}；主窗口可见={}；前台={}；折叠区仍展开={overflow}",IsWindowVisible(main).as_bool(),GetForegroundWindow()==main);
        assert!(result.is_ok() && IsWindowVisible(main).as_bool() && !IsHungAppWindow(main).as_bool() && !overflow);
    }}
    #[test]
    fn capture_overlays_do_not_count_as_viewing_a_messenger(){
        use crate::app::Messenger;
        let main=(WS_SYSMENU|WS_MINIMIZEBOX).0;
        assert!(client_main_frame(Messenger::Qq,"Chrome_WidgetWin_1",main,0));
        assert!(client_main_frame(Messenger::Wechat,"Qt51514QWindowIcon",main,0));
        assert!(!client_main_frame(Messenger::Qq,"Chrome_WidgetWin_1",WS_POPUP.0,WS_EX_TOPMOST.0));
        assert!(!client_main_frame(Messenger::Wechat,"Qt51514QWindowIcon",WS_POPUP.0,0));
        assert!(!client_main_frame(Messenger::Wechat,"Screenshot",main,0));
    }
    #[test]
    fn weixin_qt_main_window_without_system_menu_counts_as_viewed(){
        use crate::app::Messenger;
        assert!(client_main_frame(Messenger::Wechat,"Qt51514QWindowIcon",0x96c70000,0x100));
        assert!(!client_main_frame(Messenger::Wechat,"Qt51514QWindowIcon",WS_POPUP.0,0x100));
    }
    #[test]
    fn qq_nt_main_window_without_system_menu_counts_as_viewed(){
        use crate::app::Messenger::Qq;
        assert!(client_main_frame(Qq,"Chrome_WidgetWin_1",0x14c70000,0x100));
        assert!(!client_main_frame(Qq,"Chrome_WidgetWin_1",0x04c20000,0x80108));
        assert!(!client_main_frame(Qq,"Chrome_WidgetWin_1",0x04030000,0x80000));
    }
    #[test]
    #[ignore="需要本机 QQ 主窗口实际位于前台"]
    fn local_qq_main_window_is_viewed(){
        assert!(client_in_foreground(crate::app::Messenger::Qq));
    }
    #[test]
    fn protocol_registration_requires_an_absolute_wechat_executable(){
        assert_eq!(protocol_executable(r#"D:\Weixin\Weixin.exe "%1""#),Some(PathBuf::from(r"D:\Weixin\Weixin.exe")));
        assert_eq!(protocol_executable(r#""D:\Program Files\WeChat.exe" "%1""#),Some(PathBuf::from(r"D:\Program Files\WeChat.exe")));
        assert!(protocol_executable("Weixin.exe %1").is_none());
        assert!(protocol_executable(r#""C:\Windows\cmd.exe" /c Weixin.exe"#).is_none());
    }
    #[test]
    #[ignore="需要已启动并位于后台的微信窗口；仅向测试窗口发送模拟事件"]
    fn local_wechat_background_window(){unsafe{
        let target:isize=std::env::var("DAOZAI_TEST_WECHAT_HWND").unwrap().parse().unwrap();
        let listener=Listener::new(Default::default()).unwrap();
        let hwnd=listener.hwnd;
        let message=RegisterWindowMessageW(w!("SHELLHOOK"));
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(target));assert!(!listener.take().is_empty());
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(target));assert!(listener.take().is_empty());
        drop(listener);assert!(!IsWindow(hwnd).as_bool());
    }}
    #[test]
    fn shell_notifications_filter_targets_merge_flashes_and_detach(){unsafe{
        let hwnd=CreateWindowExW(WS_EX_TOOLWINDOW,w!("STATIC"),w!(""),WS_POPUP,0,0,0,0,None,None,None,None).unwrap();
        let message=RegisterWindowMessageW(w!("SHELLHOOK"));
        let state=Box::new(RefCell::new(State{message,taskbar_created:RegisterWindowMessageW(w!("TaskbarCreated")),registered:true,paths:vec![],burst:Vec::new(),pending:Vec::new(),viewing:Vec::new(),ctx:Default::default()}));
        SetWindowSubclass(hwnd,Some(subclass),SUBCLASS_ID,(&*state as *const RefCell<State>) as usize).ok().unwrap();
        RegisterShellHookWindow(hwnd).ok().unwrap();
        let listener=Listener{hwnd,state};
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(hwnd.0 as isize));
        assert!(listener.take().is_empty(),"其他程序的闪动不得触发微信提醒");
        listener.state.borrow_mut().paths=vec![normalized(&std::env::current_exe().unwrap().to_string_lossy())];
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(0));assert!(listener.take().is_empty());
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(hwnd.0 as isize));
        assert_eq!(listener.take(),[(std::process::id(),hwnd.0 as isize)],"提醒带上账号进程和闪动的窗口");
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(hwnd.0 as isize));assert!(listener.take().is_empty());
        SendMessageW(hwnd,message,WPARAM(4),LPARAM(hwnd.0 as isize));
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(hwnd.0 as isize));assert!(listener.take().is_empty());
        listener.state.borrow_mut().burst.clear();
        SendMessageW(hwnd,message,WPARAM(FLASH),LPARAM(hwnd.0 as isize));
        SendMessageW(hwnd,message,WPARAM(0x8004),LPARAM(hwnd.0 as isize));
        assert!(!listener.take().is_empty(),"后台窗口的激活事件应保留尚未消费的提醒");
        DeregisterShellHookWindow(hwnd).ok().unwrap();
        SendMessageW(hwnd,RegisterWindowMessageW(w!("TaskbarCreated")),WPARAM(0),LPARAM(0));
        assert!(listener.registered());
        drop(listener);assert!(!IsWindow(hwnd).as_bool());
    }}
    #[test]
    fn continuous_flashes_are_one_reminder_and_quiet_gap_rearms(){
        let start=Instant::now();let mut burst=Burst::default();
        assert!(burst.attention(start));
        for second in 1..30 {assert!(!burst.attention(start+Duration::from_secs(second)));}
        assert!(burst.attention(start+Duration::from_secs(34)));
        burst=Burst::default();
        assert!(burst.attention(start+Duration::from_secs(35)));
    }
    #[test]
    fn wechat_is_recognized_by_tencent_file_name_when_registry_path_is_stale(){
        // 注册表里的旧路径对不上时，仍按「文件名 + 腾讯发布」识别；其它公司的同名程序不算。
        let stale=vec![normalized(r"C:\Old\Weixin\Weixin.exe"),"*weixin.exe".to_string()];
        let me=normalized(&std::env::current_exe().unwrap().to_string_lossy());
        assert!(!path_allowed(&me,&stale),"测试程序不是腾讯发布的微信");
        assert!(path_allowed(&normalized(r"C:\Old\Weixin\Weixin.exe"),&stale),"注册表路径仍直接认");
        assert!(!path_allowed(&me,&[]));
        // 本机装了微信时，用它的真实程序验证：路径列表里只有过期路径也能认出来。
        if let Some(real)=installed_paths().into_iter().find(|p|!p.starts_with('*')){
            assert!(company_name(&real).to_lowercase().contains("tencent"),"本机微信的公司名：{}",company_name(&real));
            assert!(path_allowed(&real,&stale));
            assert!(!path_allowed(&real,&[normalized(r"C:\Old\Weixin\Weixin.exe"),"*qq.exe".into()]),"文件名不对不得认作其它客户端");
        }
    }
    #[test]
    fn flashes_right_after_leaving_a_viewed_account_do_not_remind_again(){
        // 群友反馈：最小化收到消息 → Alt+Tab 过去看了 → 再 Alt+Tab 切走，未读又出现。
        let start=Instant::now();
        let mut burst=Burst::left(start);
        assert!(!burst.attention(start+Duration::from_millis(200)),"切走时的补闪不算新提醒");
        assert!(!burst.attention(start+Duration::from_secs(3)),"宽限期内的闪动不算");
        for second in 4..20 {assert!(!burst.attention(start+Duration::from_secs(second)),"补闪持续不断时继续合并：{second}s");}
        assert!(burst.attention(start+Duration::from_secs(26)),"安静五秒后的新消息照常提醒");
        let mut quiet=Burst::left(start);
        assert!(quiet.attention(start+Duration::from_secs(10)),"切走后过了宽限期且没有补闪，新消息照常提醒");
    }
    #[test]
    fn paths_match_full_install_location(){
        assert_eq!(normalized(r"\\?\D:\Apps\Weixin.exe"),normalized("d:/apps/weixin.exe"));
        assert_ne!(normalized(r"D:\Apps\Weixin.exe"),normalized(r"D:\Other\Weixin.exe"));
    }
}
