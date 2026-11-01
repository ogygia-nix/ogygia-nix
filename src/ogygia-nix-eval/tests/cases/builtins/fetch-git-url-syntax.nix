# URLs that are valid, or that are SCP-style and so not parsed as URLs,
# reach rev.
let
  caught = url: !(builtins.tryEval (fetchGit {
    inherit url;
    rev = throw "x";
  })).success;
in
map caught [
  "https://u:p:q@h!$&'()*+,;=~_-.:080/a;b=c:@/%41%aB?[]/?#a#b?/:@"
  "https://:65535"
  "https://h:/"
  "https://"
  "https:"
  "a+b-c.d://h/x"
  "file://"
  "file:///x"
  "file://u@:1/x"
  "git+file://:1/x"
  "a+b+file://h"
  "file+https://h"
  "x+FILE://h"
  "https://h%20b/"
  "https://[::1%25a%41]/"
  "https://[v1.x:y]:80/"
  "https://[::]/"
  "https://[1::]/"
  "https://[1:2:3:4:5:6:7::]/"
  "https://[::1:2:3:4:5:6:7]/"
  "https://[1:2:3:4:5:6:1.2.3.4]/"
  "https://[::ffff:255.0.10.4]/"
  "https://u@[::1]/"
  "FILE:a b"
  "foo:/a b"
  "x:a b//"
  "a_b:x y"
  "git+foo:a b"
  "[::1]:b"
  "u@[::ffff:1.2.3.4]:b c"
  "[::1]:b@c"
  "a:b@c:"
  "[x:b"
  "x[a]:b"
  "u[@a]:b"
  "a:[x]"
  "[@a]:b"
  "[a@b]:c"
  "u@v@[a]:b"
  "u@a]b@[c]:x"
  "u@[::1]:b@[c]:d"
  "[a]:b@[::1]:c"
  "[::1]:b@["
]
