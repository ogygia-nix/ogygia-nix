# Unknown Git transports without // are SCP-style host:path references.
# The host retains git+, and the already-normalised URL path/query becomes
# literal SSH path text. Known transports and URLs with // stay URLs.
[
  (builtins.parseFlakeRef git+:a)
]
++ map builtins.parseFlakeRef [
  "git+lttp:s"
  "git+a:/s"
  "git+a:/"
  "git+a:s/t"
  "git+a:s:t"
  "git+a:./s/../t"
  "git+a:~u/s"
  "git+a:%61%2f%20%25"
  "git+a:a+b@c"
  "git+a:s?ref=main&rev=r&dir=d&shallow=1&lfs=0&narHash=n&z=1"
  "git+a:x?x=a+b&y=%61%2f"
  "git+a:?x="
  "git+HTTP:s"
  "git+rsync:s"
  "git+git+https:s"
  "git+http:s"
  "git+https:s"
  "git+ssh:s"
  "git+git:s"
  "git+file:s"
  "git+ftp:s"
  "git+ftps:s"
  "git+a://h/s"
  "git+a://u@h:2/s"
  "git+a://"
  "git+a:///s"
  "git+HTTP://h/s"
  "git+rsync://h/s"
  "git+git+https://h/s"
]
