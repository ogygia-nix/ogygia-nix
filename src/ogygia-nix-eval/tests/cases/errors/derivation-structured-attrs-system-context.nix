(derivation {
  name = "b";
  system = (derivation {
    name = "i";
    system = "x";
    builder = "/bin/sh";
  }).drvPath;
  builder = "/bin/sh";
  __structuredAttrs = true;
}).drvPath
