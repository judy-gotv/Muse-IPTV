# portal — en (开发文档 §12.3, key 用点分层级)
portal-quota-used = { $used } of { $max } streams in use
portal-quota-remaining = { $count } streams remaining
portal-quota-exhausted = Concurrent stream limit reached. Stop another device and try again.
portal-quota-exhausted-title = Stream limit reached
portal-token-expired = Token expired
portal-no-channels = No channels available
portal-watch-now = Watch now
portal-plan-expires = Plan valid until { $date }
portal-device-online-since = Online since { $since }
portal-session-expires-in =
    { $days ->
        [0] Expires today
       *[other] Expires in { $days } days
    }
