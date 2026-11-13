let
  attempt = attrs: builtins.tryEval (builtins.flakeRefToString attrs);
in
[
  # Nonnegative integers, strings and Booleans allow later attributes to force.
  (attempt { type = 0; later = throw "later"; })
  (attempt { type = 9223372036854775807; later = throw "later"; })
  (attempt { type = true; later = throw "later"; })
  (attempt { type = false; later = throw "later"; })
  (attempt { type = "path"; later = throw "later"; })
  # A throw before the negative integer remains catchable.
  (attempt { type = throw "first"; builtins = -1; })
  (attempt { early = throw "first"; late = -1; })
]
