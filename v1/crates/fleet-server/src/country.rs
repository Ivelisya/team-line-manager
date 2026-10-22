//! 机器的国家（ISO 3166-1 两位代码），网页在名字旁显示国旗。上架、修改时可以手动指定，按这张表校验；
//! 没指定的按机器地址查离线 IP 库（views.rs），不落库，库更新后跟着变。

/// ISO 3166-1 正式分配的两位代码，另加科索沃 XK（IP 库里常见）
const CODES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ", "BA", "BB", "BD", "BE", "BF", "BG",
    "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS", "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI",
    "CK", "CL", "CM", "CN", "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE", "EG", "EH",
    "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF", "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ",
    "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM", "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT",
    "JE", "JM", "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC", "LI", "LK", "LR", "LS",
    "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK", "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU",
    "MV", "MW", "MX", "MY", "MZ", "NA", "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW", "SA", "SB", "SC", "SD", "SE", "SG",
    "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS", "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK",
    "TL", "TM", "TN", "TO", "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI", "VN", "VU",
    "WF", "WS", "XK", "YE", "YT", "ZA", "ZM", "ZW",
];

/// 手动填的国家：去空白、转大写，要在 ISO 3166-1 里；空串 = 不指定（按 IP 自动），返回 None
pub fn normalize(s: &str) -> Result<Option<String>, String> {
    let c = s.trim().to_ascii_uppercase();
    if c.is_empty() {
        return Ok(None);
    }
    if CODES.contains(&c.as_str()) {
        return Ok(Some(c));
    }
    Err(match c.as_str() {
        "UK" => "国家代码「UK」不对：英国是 GB".to_string(),
        _ => format!("国家代码「{}」不对：要填 ISO 3166-1 两位代码，如 US、JP、DE；不填就按 IP 自动定", s.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(CODES.len(), 250);
        assert!(CODES.windows(2).all(|w| w[0] < w[1]), "按字母排、不重复");
        assert_eq!(normalize(" us ").unwrap().as_deref(), Some("US"));
        assert_eq!(normalize("").unwrap(), None);
        assert!(normalize("UK").unwrap_err().contains("GB"));
        assert!(normalize("ZZ").is_err() && normalize("USA").is_err());
    }
}
