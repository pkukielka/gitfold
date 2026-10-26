# Prints the TREE extension (cache tree) of a git index, version 4, one sorted
# "/<path> <entries> <subtrees> <tree id>" line per node.
# usage: perl tests/cache-tree.pl <index>
undef $/; open my $f, '<:raw', $ARGV[0] or die; my $d = <$f>;
my ($sig, $ver, $n) = unpack 'a4 N N', $d; die "not an index" unless $sig eq 'DIRC';
my $p = 12; my $prev = '';
for (1..$n) {
  my $flags = unpack 'n', substr($d, $p + 60, 2); $p += 62;
  $p += 2 if $ver >= 3 && ($flags & 0x4000);
  if ($ver == 4) {
    my ($b, $v) = (0, 0); do { $b = ord substr($d, $p++, 1); $v = ($v << 7) | ($b & 127); $v++ if $b & 128 } while ($b & 128);
    my $e = index($d, "\0", $p); $prev = substr($prev, 0, length($prev) - $v) . substr($d, $p, $e - $p); $p = $e + 1;
  } else { die "index version $ver: only 4 is read" }
}
my @out;
while ($p < length($d) - 20) {
  my ($sig, $len) = unpack 'a4 N', substr($d, $p, 8); $p += 8;
  if ($sig eq 'TREE') {
    my $q = $p; my $end = $p + $len;
    my @stack;  # [path, remaining subtrees]
    while ($q < $end) {
      my $z = index($d, "\0", $q); my $name = substr($d, $q, $z - $q); $q = $z + 1;
      my $nl = index($d, "\n", $q); my ($cnt, $sub) = split / /, substr($d, $q, $nl - $q); $q = $nl + 1;
      my $oid = ''; if ($cnt >= 0) { $oid = unpack 'H*', substr($d, $q, 20); $q += 20 }
      while (@stack && $stack[-1][1] == 0) { pop @stack }
      my $path = @stack ? ($stack[-1][0] eq '' ? $name : "$stack[-1][0]/$name") : $name;
      $stack[-1][1]-- if @stack;
      push @out, "/$path $cnt $sub $oid";
      push @stack, [$path, $sub];
    }
  }
  $p += $len;
}
print "$_\n" for sort @out;
