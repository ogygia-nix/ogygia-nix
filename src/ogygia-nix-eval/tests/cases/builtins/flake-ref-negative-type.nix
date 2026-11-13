# The startup-interned type attribute is checked before builtins, even here.
builtins.tryEval (
  builtins.flakeRefToString {
    builtins = throw "later";
    type = -9223372036854775807 - 1;
  }
)
