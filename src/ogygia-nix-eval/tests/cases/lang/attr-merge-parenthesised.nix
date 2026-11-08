# Parentheses around a set literal do not stop it merging with another
# definition of the same attribute.
let
  x = 5;
in
{
  a = ({ x = 1; });
  a = (({ y = 2; }));
  b = (rec { x = 1; });
  b.y = x;
  c.p = 1;
  c = ({ q = 2; });
  d = let
    e = ({ x = 1; });
    e = ({ y = x; });
  in
  e;
}
