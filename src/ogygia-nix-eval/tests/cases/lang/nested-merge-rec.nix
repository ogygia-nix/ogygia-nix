let
  x = 5;
in
{
  plain = rec { }; plain.b = 1;
  recFirst = rec { x = 1; }; recFirst = { y = x; };
  recSecond = { x = 1; }; recSecond = rec { y = x; };
  pathFirst.y = x; pathFirst = rec { x = 1; };
  bothRec = rec { x = 1; }; bothRec = rec { y = x; };
  deep = rec { x = 1; }; deep.b.c = x;
  inner = { b = rec { x = 1; }; }; inner.b.y = x;
  dyn = rec { x = "z"; }; dyn = { ${x} = 2; inherit ({ y = x; }) y; };
  lets = let a = rec { x = 1; }; a.y = x; x = 7; in a;
  recs = rec { a = rec { x = 1; }; a.y = x; x = 7; };
}
