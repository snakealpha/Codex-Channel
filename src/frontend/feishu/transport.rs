pub fn extract_service_id(url: &str) -> Option<i32> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == "service_id" {
            return value.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::extract_service_id;

    #[test]
    fn extracts_service_id_from_ws_url() {
        assert_eq!(
            extract_service_id("wss://example.com/ws?foo=1&service_id=42&bar=2"),
            Some(42)
        );
    }
}
