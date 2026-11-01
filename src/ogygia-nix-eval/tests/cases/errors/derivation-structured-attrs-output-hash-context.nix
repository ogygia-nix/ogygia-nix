(derivation {
  name = "b";
  system = "x";
  builder = "/bin/sh";
  __structuredAttrs = true;
  outputHashMode = "flat";
  outputHashAlgo = "sha256${builtins.substring 0 0 (builtins.toFile "f" "x")}";
  outputHash = "0000000000000000000000000000000000000000000000000000";
}).drvPath
