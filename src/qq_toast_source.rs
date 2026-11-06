//! 仅在已授权的系统通知缺少应用标识时，按编号和到达时间核对 Windows 记录的发送方。
use std::ffi::{c_char,c_int,c_void,CString};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

type RowCallback=unsafe extern "C" fn(*mut c_void,c_int,*mut *mut c_char,*mut *mut c_char)->c_int;
type OpenV2=unsafe extern "C" fn(*const c_char,*mut *mut c_void,c_int,*const c_char)->c_int;
type Exec=unsafe extern "C" fn(*mut c_void,*const c_char,Option<RowCallback>,*mut c_void,*mut *mut c_char)->c_int;
type Close=unsafe extern "C" fn(*mut c_void)->c_int;

// 上游原版用 #[link(name="winsqlite3")] 链接 Windows SDK 的导入库；本机 GNU/MinGW
// 工具链没有 libwinsqlite3.a。改为运行时从系统自带的 winsqlite3.dll 取这三个入口，
// 功能不变，且不需要任何额外导入库或构建环境变量。
#[link(name="kernel32")]
extern "system" {
    fn LoadLibraryW(name:*const u16)->isize;
    fn GetProcAddress(module:isize,name:*const c_char)->*const c_void;
}

struct Api{open_v2:OpenV2,exec:Exec,close:Close}
static API:OnceLock<Option<Api>>=OnceLock::new();
fn api()->Option<&'static Api>{
    API.get_or_init(||unsafe{
        let dll:Vec<u16>="winsqlite3.dll".encode_utf16().chain(std::iter::once(0)).collect();
        let module=LoadLibraryW(dll.as_ptr());
        if module==0{return None;}
        // 进程生命周期内保留句柄，不 FreeLibrary。
        let get=|name:&'static [u8]|GetProcAddress(module,name.as_ptr().cast());
        let a=get(b"sqlite3_open_v2\0");let b=get(b"sqlite3_exec\0");let c=get(b"sqlite3_close\0");
        if a.is_null()||b.is_null()||c.is_null(){return None;}
        Some(Api{
            open_v2:std::mem::transmute::<*const c_void,OpenV2>(a),
            exec:std::mem::transmute::<*const c_void,Exec>(b),
            close:std::mem::transmute::<*const c_void,Close>(c),
        })
    }).as_ref()
}

pub(super) fn matches(id:u32,created:i64)->bool{
    let Some(local)=std::env::var_os("LOCALAPPDATA") else{return false};
    matches_in(&Path::new(&local).join("Microsoft/Windows/Notifications/wpndatabase.db"),id,created)
}

fn matches_in(path:&Path,id:u32,created:i64)->bool{
    let Ok(name)=CString::new(path.to_string_lossy().as_bytes()) else{return false};
    // 查询只包含数值参数。两个字段同时匹配，避免复用编号时串到另一条通知。
    let sql=CString::new(format!("SELECT 1 FROM Notification n JOIN NotificationHandler h ON h.RecordId=n.HandlerId WHERE n.Id={id} AND n.ArrivalTime={created} AND n.Type='toast' AND h.PrimaryId='QQ' LIMIT 1")).unwrap();
    unsafe extern "C" fn found(data:*mut c_void,_:c_int,_:*mut *mut c_char,_:*mut *mut c_char)->c_int{
        unsafe { *data.cast::<bool>()=true; } 0
    }
    let mut db=ptr::null_mut();
    let mut matched=false;
    let Some(api)=api() else{return false;};
    unsafe{
        const SQLITE_OPEN_READONLY:c_int=1;
        let opened=(api.open_v2)(name.as_ptr(),&mut db,SQLITE_OPEN_READONLY,ptr::null())==0;
        let ok=opened && (api.exec)(db,sql.as_ptr(),Some(found),(&mut matched as *mut bool).cast(),ptr::null_mut())==0;
        if !db.is_null(){(api.close)(db);}
        ok && matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore="需要本机通知中心留有指定测试通知"]
    fn qq_notification_local_identity(){
        let id=std::env::var("DAOZAI_QQ_TEST_ID").unwrap().parse().unwrap();
        use windows::UI::Notifications::{Management::UserNotificationListener,NotificationKinds};
        use windows::Win32::System::WinRT::{RoInitialize,RoUninitialize,RO_INIT_MULTITHREADED};
        unsafe { RoInitialize(RO_INIT_MULTITHREADED).unwrap(); }
        let items=UserNotificationListener::Current().unwrap().GetNotificationsAsync(NotificationKinds::Toast).unwrap().get().unwrap();
        let n=(0..items.Size().unwrap()).filter_map(|i|items.GetAt(i).ok()).find(|n|n.Id().ok()==Some(id)).expect("原始测试通知已离开通知中心");
        let created=n.CreationTime().unwrap().UniversalTime;
        assert_eq!(super::super::notification_identity(&n,id,created,false),("QQ".into(),"QQ".into()));
        assert!(matches(id,created));
        drop(n);drop(items);unsafe { RoUninitialize(); }
        assert!(!matches(id,created+1));
    }
}
