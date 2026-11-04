//! WorkBuddy（腾讯 CodeBuddy）积分余额。
//!
//! 设计边界（与任务书锁定决策一致）：
//! - 只读本机登录文件 `.workbuddy-accounts.json`，绝不写回、绝不做 token 自动续期。
//! - 积分是「绝对值」，没有可靠分母，因此只产出 `Reading`，圆环不画百分比弧。
//! - 一个登录文件可能含多个账号，用 `uid` 作为 slot 定位，结构与 Grok Bot 的 slot 同构。
use super::{Account, Reading, Snapshot};
use chrono::Datelike;
use serde_json::{json, Value};
use std::{fs, io::Read, path::{Path, PathBuf}};

/// 登录失效统一文案；401 与本地过期预判都用它，避免在各处拼出不一致的句子。
const LOGIN_ERROR: &str = "WorkBuddy 登录已过期，请在 WorkBuddy 里重新登录";
const ENDPOINT: &str = "https://www.codebuddy.cn/v2/billing/meter/get-user-resource";
/// 照抄 DSH 插件口径：查询窗口结束 = 开始 + 3185136000 ms（约 36.86 天）。
const RANGE_SPAN_MS: i64 = 3_185_136_000;

/// 本机登录文件位置。
///
/// 依次试：`$DSH_HOME/.workbuddy-accounts.json` → `%USERPROFILE%/DSH-Data/.workbuddy-accounts.json`。
/// `DSH_HOME` 是用户级环境变量，某些 shell 里可能缺失，所以必须保留 USERPROFILE 兜底。
pub fn native_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("DSH_HOME").filter(|p| !p.is_empty()) {
        let candidate = PathBuf::from(home).join(".workbuddy-accounts.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("DSH-Data")
        .join(".workbuddy-accounts.json");
    fallback.is_file().then_some(fallback)
}

/// 一个可导入的账号轮廓：slot 用 uid，label 带昵称。
pub struct Profile {
    pub slot: String,
    pub label: String,
}

/// 从登录文件读出全部账号轮廓。slot = uid，label = `WorkBuddy · {nickname}`。
pub fn profiles(path: &Path) -> Result<Vec<Profile>, String> {
    let raw = fs::read(path).map_err(|_| "未找到 WorkBuddy 登录文件".to_string())?;
    let root: Value = serde_json::from_slice(&raw).map_err(|_| "WorkBuddy 登录文件不是有效的 JSON".to_string())?;
    let rows = root["accounts"].as_array().ok_or("WorkBuddy 登录文件里没有账号")?;
    let mut out = Vec::new();
    for entry in rows {
        let uid = entry["uid"].as_str().filter(|s| !s.is_empty()).ok_or("WorkBuddy 账号缺少 uid")?;
        let nickname = entry["nickname"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("账号");
        out.push(Profile { slot: uid.to_owned(), label: format!("WorkBuddy · {nickname}") });
    }
    if out.is_empty() {
        return Err("WorkBuddy 登录文件里没有可用账号".into());
    }
    Ok(out)
}

/// 兼容字符串/数字两种 JSON 取值；非有限值一律视为缺失。
fn field_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(_) => v.as_f64().filter(|n| n.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

/// 取一个容量字段：`Precise`（字符串、可能带小数）优先，解析失败再退回普通数字字段。
/// 两个都解析不出来时返回 None，由调用方决定是否跳过整个包。
fn package_number(pkg: &Value, precise: &str, plain: &str) -> Option<f64> {
    field_number(&pkg[precise]).or_else(|| field_number(&pkg[plain]))
}

/// 解析 `CycleEndTime`（本地时间 `YYYY-MM-DD HH:MM:SS`）；空串或格式不符返回 None。
fn cycle_end(pkg: &Value) -> Option<chrono::NaiveDateTime> {
    let s = pkg["CycleEndTime"].as_str().filter(|s| !s.trim().is_empty())?;
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()
}

/// 纯解析（不联网），单元测试直接调用。
///
/// 入参是响应里的 `data.Response.Data` 层：`{"TotalCount","TotalDosage","Accounts"}`。
/// 口径照任务书：
/// `remain = Σ max(0, CapacityRemainPrecise ?? CapacityRemain)`；
/// 某包两个剩余字段都缺失则跳过该包（不当 0，避免一个字段异常拉低总数）。
pub fn parse(v: &Value) -> Result<Snapshot, String> {
    let accounts = v["Accounts"].as_array().ok_or("WorkBuddy 未返回积分包数据")?;
    let now = chrono::Local::now().naive_local();
    let mut remain = 0.0f64;
    let mut size = 0.0f64;
    let mut counted = 0usize;
    let mut next_cycle: Option<chrono::NaiveDateTime> = None;
    for pkg in accounts {
        // 剩余字段是「是否计入该包」的依据：两个都缺就跳过整个包。
        let Some(r) = package_number(pkg, "CapacityRemainPrecise", "CapacityRemain") else { continue; };
        if let Some(s) = package_number(pkg, "CapacitySizePrecise", "CapacitySize") {
            size += s.max(0.0);
        }
        remain += r.max(0.0);
        counted += 1;
        // 只在仍生效的包里找「未来最早」的到期时间，耗尽包的过去时间不参与。
        if pkg["Status"].as_i64() != Some(3) {
            if let Some(ct) = cycle_end(pkg).filter(|ct| *ct > now) {
                next_cycle = Some(next_cycle.map_or(ct, |cur| cur.min(ct)));
            }
        }
    }
    // 0 是有意义的读数（全包耗尽），返回 Ok 而不是 Err，与「没有可用窗口」的错误区分。
    let caption = if remain <= 0.0 {
        "积分已用完".to_string()
    } else {
        match next_cycle {
            Some(ct) => format!("{counted} 个积分包 · 最近到期 {m}月{d}日", m = ct.month(), d = ct.day()),
            None => format!("{counted} 个积分包"),
        }
    };
    let _ = size; // 总量本版不参与绘制（无分母），保留计算以备后续，避免编造假进度。
    Ok(Snapshot {
        plan: "WorkBuddy".into(),
        windows: vec![],
        extra: vec![],
        reading: Some(Reading { value: remain, unit: "分", caption }),
        at: chrono::Utc::now().timestamp(),
    })
}

/// 把 UTC 毫秒时间戳转成接口要求的本地时间字符串（无时区后缀）。
fn local_string(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// 读取响应体，限制 1 MiB，防止异常超大响应占内存。写法对齐 grok_quota 的 read_json。
fn read_json(response: ureq::Response) -> Result<Value, String> {
    let mut body = Vec::new();
    response.into_reader().take(1024 * 1024 + 1).read_to_end(&mut body).map_err(|_| "读取 WorkBuddy 响应失败".to_string())?;
    if body.len() > 1024 * 1024 {
        return Err("WorkBuddy 响应过大".into());
    }
    serde_json::from_slice(&body).map_err(|_| "WorkBuddy 未返回有效数据".into())
}

/// HTTP 错误 →（文案，退避秒数）。退避口径照任务书与 quota.rs 现有写法。
fn http_error(error: ureq::Error) -> (String, u64) {
    match error {
        ureq::Error::Status(401, _) => (LOGIN_ERROR.into(), 0),
        // 403 不等于登录失效：网关拦截异常客户端也返回 403。混在一起会把
        // 「UA 被拦」误报成「请重新登录」，让人白重登一次。
        ureq::Error::Status(403, _) => ("WorkBuddy 拒绝了这次请求（403），请稍后重试".into(), 60),
        ureq::Error::Status(429, r) => (
            "请求过于频繁，稍后自动重试".into(),
            r.header("Retry-After").and_then(|s| s.parse().ok()).unwrap_or(180).clamp(30, 3600),
        ),
        ureq::Error::Status(_, _) => ("WorkBuddy 额度服务暂不可用，稍后自动重试".into(), 60),
        _ => ("无法连接 WorkBuddy 服务，请检查网络或代理".into(), 30),
    }
}

/// 主动刷新：岛仔自己调腾讯接口，能独立刷新，不依赖 DSH 是否开着。
pub fn fetch(agent: &ureq::Agent, account: &Account) -> Result<Snapshot, (String, u64)> {
    let slot = account.slot.as_deref().ok_or_else(|| ("WorkBuddy 账号信息不完整，请重新导入".into(), 0))?;
    let raw = fs::read(&account.source).map_err(|_| (LOGIN_ERROR.into(), 0))?;
    let root: Value = serde_json::from_slice(&raw).map_err(|_| (LOGIN_ERROR.into(), 0))?;
    // 一个文件可能有多个账号，用 slot(uid) 定位本账号。
    let entry = root["accounts"]
        .as_array()
        .and_then(|rows| rows.iter().find(|a| a["uid"].as_str() == Some(slot)))
        .ok_or_else(|| (LOGIN_ERROR.into(), 0))?;
    let access_token = entry["accessToken"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| (LOGIN_ERROR.into(), 0))?;
    let uid = entry["uid"].as_str().unwrap_or(slot);
    let domain = entry["domain"].as_str().filter(|s| !s.is_empty()).unwrap_or("www.codebuddy.cn");
    // 请求前先看 expiresAtMs：已过期就直接返回，不白发一次请求。
    let expires = entry["expiresAtMs"].as_i64().unwrap_or(0);
    if expires <= chrono::Utc::now().timestamp_millis() {
        return Err((LOGIN_ERROR.into(), 0));
    }
    let begin_ms = chrono::Utc::now().timestamp_millis();
    let request_body = json!({
        "PageNumber": 0,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": local_string(begin_ms),
        "PackageEndTimeRangeEnd": local_string(begin_ms + RANGE_SPAN_MS),
    });
    let response = agent
        .post(ENDPOINT)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("X-User-Id", uid)
        .set("X-Domain", domain)
        // 腾讯网关会拦截通用 HTTP 库的默认 UA（实测 ureq 的 `ureq/2.x` 直接 403、
        // 响应体 code=10085「请求不合法」），必须显式声明客户端身份。
        // 用自家名字而不是冒充 `WorkBuddy/x.y`：这份 token 是用户的，请求是我们发的。
        .set("User-Agent", concat!("Daozai/", env!("CARGO_PKG_VERSION")))
        .send_string(&request_body.to_string())
        .map_err(http_error)?;
    let value = read_json(response).map_err(|e| (e, 60))?;
    let data = &value["data"]["Response"]["Data"];
    if data.is_null() {
        // 结构与预期不符时兜底直接解析整体，避免因路径变化直接失败。
        return parse(&value).map_err(|e| (e, 60));
    }
    parse(data).map_err(|e| (e, 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_packages_and_ignores_exhausted() {
        // 7 个生效包剩余合计 478（100*4 + 50 + 20 + 8），5 个耗尽包剩余为 0。
        let mut packages = Vec::new();
        let active = [100.0, 100.0, 100.0, 100.0, 50.0, 20.0, 8.0];
        for (k, rem) in active.iter().enumerate() {
            packages.push(json!({
                "PackageName": format!("active-{k}"),
                "Status": 0,
                "CapacityRemainPrecise": rem.to_string(),
                "CapacitySizePrecise": "300",
                "CycleEndTime": "2026-12-01 15:37:53",
            }));
        }
        for k in 0..5 {
            packages.push(json!({
                "PackageName": format!("exhausted-{k}"),
                "Status": 3,
                "CapacityRemainPrecise": "0",
                "CapacitySizePrecise": "100",
                "CycleEndTime": "",
            }));
        }
        let v = json!({"TotalCount": 12, "TotalDosage": 478, "Accounts": packages});
        let s = parse(&v).expect("12 包样本应可解析");
        let reading = s.reading.as_ref().expect("应产出 Reading 而非百分比窗口");
        assert_eq!(reading.value, 478.0);
        assert_eq!(reading.unit, "分");
        assert!(s.windows.is_empty(), "绝对值来源不应产生百分比窗口");
    }

    #[test]
    fn all_exhausted_is_zero_not_error() {
        let packages: Vec<_> = (0..3)
            .map(|_| json!({"Status": 3, "CapacityRemainPrecise": "0", "CapacitySizePrecise": "100"}))
            .collect();
        let s = parse(&json!({"Accounts": packages})).expect("全包耗尽是有效读数，不应报错");
        let reading = s.reading.as_ref().unwrap();
        assert_eq!(reading.value, 0.0);
        assert!(reading.caption.contains("已用完"));
    }

    #[test]
    fn malformed_fields_skip_the_package() {
        let v = json!({"Accounts": [
            {"PackageName": "bad", "Status": 0},                 // 两个剩余字段都缺 → 跳过
            {"PackageName": "good", "Status": 0,
             "CapacityRemainPrecise": "12.5", "CapacityRemain": 999}, // Precise 优先
        ]});
        let s = parse(&v).unwrap();
        let reading = s.reading.as_ref().unwrap();
        assert_eq!(reading.value, 12.5, "缺字段包应跳过，且 Precise 优先于普通数字字段");
    }
}
