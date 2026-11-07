let
  x = 7;
  n = "q";
  ov = "__overrides";
  o = {
    __overrides = {
      a = 5;
    };
  };
in
[
  (rec { a = 1; b = a; __overrides = { a = 2; c = 3; }; })
  { a = 1; __overrides = { a = 2; }; }
  (let a = 1; __overrides = { a = 2; }; in a)
  (let { a = 1; body = a; __overrides = { a = 2; }; })
  (rec { a = 1; __overrides = { b = a; }; })
  (rec { ${"a"} = 1; __overrides = { a = 3; }; })
  (rec { ${"q"} = a; a = 1; __overrides = { a = 3; }; })
  (rec { a = 1; ${n} = a; __overrides = { a = 2; }; })
  (rec { a = "x"; ${a} = a; __overrides = { a = "y"; }; })
  (rec { a = 1; b = a; ${ov} = { a = 3; }; })
  (builtins.tryEval (rec { a = 1; ${a} = 1; __overrides = throw "x"; }))
  (rec { x.y = 1; z = x; __overrides = { x.w = 5; }; })
  (rec { inherit x; b = x; __overrides = { x = 3; }; })
  (rec { inherit (o) __overrides; a = 1; b = a; })
  (rec { ${"__overrides"} = { a = 3; }; a = 1; b = a; })
  (rec { a = 1; b = a; __overrides.a = 3; __overrides.c = 4; })
  (rec { inherit (__overrides) q; __overrides = { q = 1; }; })
  (rec { a = 1; b = a; __overrides = { a = 2; __overrides = { a = 9; }; }; })
  (rec { f = _: a; a = 1; __overrides = { a = 2; }; }.f 0)
  (rec { a = 1; b = 2; __overrides = { a = throw "x"; }; }).b
  ((rec { __overrides = { c = 3; }; }) ? c)
  (builtins.hasAttr "c" (rec { __overrides = { c = 3; }; }))
  (builtins.attrNames (rec { a = 1; __overrides = { c = 3; }; }))
  ((rec { a = 1; b = a; __overrides = { a = 2; }; }) // { a = 7; })
  { x = rec { a = 1; b = a; }; x.__overrides = { a = 2; }; }
  { x.a = 1; x.__overrides = { a = 2; }; }
  (builtins.tryEval (rec { a = 1; __overrides = throw "x"; }))
  (builtins.tryEval (rec { a = 1; __overrides = { a = throw "x"; }; }).a)
]
