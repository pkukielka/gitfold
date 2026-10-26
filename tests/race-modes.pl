#!/usr/bin/env perl
# usage: race-modes.pl <file>
# One process writes through a descriptor to <file> after making it read-only
# (mode 0444), which makes gitfold lift the owner's write bit for each of its
# reopens; meanwhile this process tries to open <file> for writing and stats it.
# Prints "<successful opens> <distinct modes seen>"; correct is "0 444".
use strict;
use warnings;
use Fcntl;

my $f = shift or die "usage: $0 <file>\n";
open(my $out, '>', $f) or die "$f: $!";
chmod 0444, $f or die "chmod: $!";
my $pid = fork() // die "fork: $!";
if ($pid == 0) {
    syswrite($out, "x") for 1 .. 20000;
    exit 0;
}
close $out;
my ($opened, %modes) = (0);
for (1 .. 20000) {
    if (sysopen(my $fh, $f, O_WRONLY)) { $opened++; close $fh }
    my $mode = (lstat $f)[2] & 07777;
    $modes{sprintf '%o', $mode} = 1;
}
waitpid($pid, 0);
print "$opened ", join(' ', sort keys %modes), "\n";
