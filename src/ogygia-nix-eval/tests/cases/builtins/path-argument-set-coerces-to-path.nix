let
  f = <nix/fetchurl.nix>;
in
[
  (builtins.readDir { __toString = _: <nix/.>; })
  (builtins.pathExists { outPath = f; })
  (builtins.pathExists { outPath = { __toString = _: f; }; })
  (builtins.pathExists {
    __toString = _: f;
    outPath = 1;
  })
  (builtins.hashFile "sha256" { __toString = _: { outPath = f; }; })
  (builtins.pathExists { __toString = _: "/nonexistent"; })
]
