//! DeepSeek Harness（DSH）余额。
//!
//! DSH 没有可干净调用的公开余额接口：账号登录态不能当 API Key 用，本地 HTTP 路由又需要它自己的
//! 会话 cookie。因此本模块只读 DSH 鲸鱼插件自己写的本地用量缓存 `.dshw-usage.json`，
//! 不发任何网络请求，也不读/逆向它的凭据库。
//!
//! 数值是「绝对值」（钱，单位元），只产出 `Reading`，不画百分比弧。
use super::{Reading, Snapshot};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

/// 缓存文件位置。
///
/// 依次试：`$DSH_HOME/.dshw-usage.json` → `%USERPROFILE%/DSH-Data/.dshw-usage.json`。
/// 保留 USERPROFILE 兜底，因为 `DSH_HOME` 在某些 shell 里可能缺失。
pub fn native_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("DSH_HOME").filter(|p| !p.is_empty()) {
        let candidate = PathBuf::from(home).join(".dshw-usage.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("DSH-Data")
        .join(".dshw-usage.json");
    fallback.is_file().then_some(fallback)
}

/// 纯解析（不联网）。`mtime` 是缓存文件的修改时间，用于判断数据是否陈旧。
///
/// 映射（本版只读顶层字段，不展开 `accounting.books`）：
/// - `value = lastBalance`，单位「元」；
/// - caption 用 `todayUsage` 写「今日已用 N 元」，缺失则写「今日用量未知」，不编 0；
/// - mtime 距今超过 24 小时，caption 追加「（数据为 N 小时前）」，缓存陈旧不算失败。
pub fn parse(v: &Value, mtime: Option<SystemTime>) -> Result<Snapshot, String> {
    let balance = match v["lastBalance"].as_f64() {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => return Err("DSH 还没有余额记录，先打开一次 DSH 再刷新".into()),
    };
    let mut caption = match v["todayUsage"].as_f64().filter(|n| n.is_finite()) {
        Some(used) => format!("今日已用 {:.2} 元", used),
        None => "今日用量未知".to_string(),
    };
    // 缓存陈旧只标注、不报错：关掉 DSH 后数值停在最后一次刷新，用户需要知道它不是实时的。
    if let Some(mtime) = mtime {
        if let Ok(elapsed) = SystemTime::now().duration_since(mtime) {
            if elapsed > Duration::from_secs(24 * 3600) {
                let hours = (elapsed.as_secs() as f64 / 3600.0).round() as u64;
                caption.push_str(&format!("（数据为 {hours} 小时前）"));
            }
        }
    }
    Ok(Snapshot {
        plan: "DSH".into(),
        windows: vec![],
        extra: vec![],
        reading: Some(Reading { value: balance, unit: "元", caption }),
        at: chrono::Utc::now().timestamp(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn reading_comes_from_last_balance() {
        let v = json!({"date": "2026-11-04", "lastBalance": 22.877404, "todayUsage": 3.982531});
        let s = parse(&v, None).expect("正常缓存应可解析");
        let r = s.reading.as_ref().expect("应产出 Reading");
        assert_eq!(r.value, 22.877404);
        assert_eq!(r.unit, "元");
        assert!(r.caption.contains("3.98"), "caption 应含两位小数今日用量：{}", r.caption);
        assert_eq!(s.plan, "DSH");
    }

    #[test]
    fn missing_balance_is_an_error() {
        // 缺失、null、负数、非数字：都无法构成有效余额，必须报错而不是显示 0。
        for v in [
            json!({}),
            json!({"lastBalance": null}),
            json!({"lastBalance": -1.0}),
            json!({"lastBalance": "not-a-number"}),
        ] {
            assert!(parse(&v, None).is_err(), "该输入应报错：{v}");
        }
        // todayUsage 缺失不应导致失败，只把说明改成「未知」。
        let s = parse(&json!({"lastBalance": 22.88}), None).unwrap();
        assert!(s.reading.as_ref().unwrap().caption.contains("今日用量未知"));
    }

    #[test]
    fn stale_file_is_captioned_not_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "daozai-harness-stale-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("stale.json");
        fs::write(&p, r#"{"lastBalance":22.88,"todayUsage":3.98}"#).unwrap();
        // 把 mtime 人为改成 30 小时前。
        let old = SystemTime::now() - Duration::from_secs(30 * 3600);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let mtime = fs::metadata(&p).unwrap().modified().unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let s = parse(&v, Some(mtime)).expect("陈旧缓存应返回 Ok，而不是拒绝");
        assert!(s.reading.as_ref().unwrap().caption.contains("小时前"));
        let _ = fs::remove_dir_all(&dir);
    }
}
