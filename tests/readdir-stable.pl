#!/usr/bin/env perl
# usage: readdir-stable.pl <new-dir> <delete|create|rename>
# Creates 3000 files in <new-dir>, reads part of its listing, then deletes 100
# entries already read, creates 100 that sort before all by name (before the
# reader, when listings were in name order) or renames 100 already read, and
# reads on. Every untouched entry must be seen exactly once (as POSIX requires
# of a directory changing while it is read; make reads directories lazily
# while parallel jobs change them). Prints "<missing> <seen twice>"; correct
# is "0 0". With positions as offsets, each mode failed.
use strict;
use warnings;

my ($d, $mode) = @ARGV;
die "usage: $0 <new-dir> <delete|create|rename>\n" unless $mode;
mkdir $d or die "mkdir $d: $!";
my $n = 3000;
my $name = sub { sprintf "%s/f%04d", $d, shift };
for my $i (0 .. $n - 1) { open(my $f, '>', $name->($i)) or die "$!"; close $f; }
opendir(my $dh, $d) or die "$!";
my (%seen, @read, %touched);
while (defined(my $e = readdir $dh)) {
    next if $e =~ /^\.\.?$/;
    $seen{$e}++;
    push @read, $e;
    next unless @read == 200;
    for my $i (0 .. 99) {
        if ($mode eq 'delete') {
            unlink "$d/$read[$i]" or die "$!";
            $touched{$read[$i]} = 1;
        } elsif ($mode eq 'create') {
            open(my $f, '>', "$d/a$i") or die "$!";
            close $f;
        } else {
            rename "$d/$read[$i]", "$d/renamed$i" or die "$!";
            $touched{$read[$i]} = 1;
        }
    }
}
my @untouched = grep { !$touched{$_} } map { sprintf "f%04d", $_ } 0 .. $n - 1;
my $missing = grep { !$seen{$_} } @untouched;
my $twice = grep { ($seen{$_} // 0) > 1 } @untouched;
print "$missing $twice\n";
