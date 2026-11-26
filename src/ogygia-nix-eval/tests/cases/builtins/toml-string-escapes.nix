# Literal strings and escaped backslashes do not decode to NUL.
map builtins.fromTOML [
  "'\\u0000'=2"
  "a='\\u0000'"
  "\"\\\\u0000\"=2"
  "a=\"\\\\u0000\""
  "\"\\u0001\"=\"\\u0001\""
]
