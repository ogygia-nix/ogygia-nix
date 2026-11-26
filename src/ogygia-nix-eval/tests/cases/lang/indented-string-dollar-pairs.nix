# Dollar pairs stay in the same plain-text piece of an indented string,
# including before a brace or quote. They therefore form static names.
let
  ${''$$''} = 1;
  source = {
    "$$" = 2;
    "$${x}" = 3;
  };
in
[
  (builtins.attrNames { inherit ({ }) ${''$$''}; })
  { inherit (source) ${''$$''} ${(''$${x}'')}; }
  { inherit ${''$$''}; }
  { ${''$${x}''}.a = 1; ${''$${x}''}.b = 2; }
  (builtins.attrNames {
    inherit ({ })
      ${''$$$$''}
      ${''a$${x}b''}
      ${''$$$${x}''}
      ${''$$'a''}
      ${''
        $$
          a
      ''}
      ;
  })
  # An unpaired dollar still starts an interpolation or its own piece.
  ''$$${"a"}''
  ''$$$''
  ''a$''
]
