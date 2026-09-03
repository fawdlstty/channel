pub(crate) trait LocalWebSocket {
    fn normalize_local_websocket(&self) -> Result<String, String>;
}

impl LocalWebSocket for str {
    fn normalize_local_websocket(&self) -> Result<String, String> {
        let endpoint = self.trim();
        let authority = endpoint
            .strip_prefix("ws://")
            .ok_or_else(|| "only local ws:// endpoints are supported".to_owned())?;
        if authority.contains(['@', '/', '?', '#']) {
            return Err("endpoint credentials, paths, and queries are not supported".to_owned());
        }
        let host = if authority == "::1" {
            "[::1]".to_owned()
        } else {
            authority.to_owned()
        };
        let (hostname, port) = if let Some(rest) = host.strip_prefix("[::1]") {
            if rest.is_empty() {
                ("[::1]", None)
            } else {
                let port = rest
                    .strip_prefix(':')
                    .ok_or_else(|| "invalid endpoint authority".to_owned())?;
                ("[::1]", Some(port))
            }
        } else if let Some((candidate, port)) = host.rsplit_once(':') {
            (candidate, Some(port))
        } else {
            (host.as_str(), None)
        };
        if hostname != "127.0.0.1" && hostname != "[::1]" && hostname != "localhost" {
            return Err("only 127.0.0.1, ::1, and localhost are allowed".to_owned());
        }
        if let Some(port) = port {
            let port = port
                .parse::<u16>()
                .map_err(|_| "invalid endpoint port".to_owned())?;
            if port == 0 {
                return Err("endpoint port must be greater than zero".to_owned());
            }
        }
        Ok(format!("ws://{host}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_only_local_websocket_endpoints() {
        assert_eq!(
            " ws://localhost:4500 ".normalize_local_websocket().unwrap(),
            "ws://localhost:4500"
        );
        assert_eq!(
            "ws://::1".normalize_local_websocket().unwrap(),
            "ws://[::1]"
        );
        assert_eq!(
            "ws://[::1]:4501".normalize_local_websocket().unwrap(),
            "ws://[::1]:4501"
        );
    }

    #[test]
    fn rejects_non_local_or_invalid_websocket_endpoints() {
        for endpoint in [
            "http://localhost:4500",
            "ws://example.invalid:4500",
            "ws://localhost:4500/path",
            "ws://user@localhost:4500",
            "ws://localhost:0",
            "ws://localhost:bad",
        ] {
            assert!(endpoint.normalize_local_websocket().is_err());
        }
    }
}
