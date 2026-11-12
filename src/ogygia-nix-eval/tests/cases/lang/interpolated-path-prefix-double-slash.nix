[
  /a//.${"b"}
  /a//..${"b"}
  ./a//.${"b"}
  ./a//..${"b"}
  (baseNameOf ~/a//.${"b"})
  (baseNameOf ~/a//..${"b"})

  # Single-slash prefixes are still normalised before interpolation.
  /a/.${"b"}
  /a/..${"b"}
  /a/..//.${"b"}

  # The string portion is normalised only after concatenation.
  /a//./..${"b"}
  /a//../.${"b"}
  /a//..${""}
  /a//..${"b"}/${"c"}
  /a//${"b"}
]
