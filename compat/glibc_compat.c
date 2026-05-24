// Shims for __isoc23_* symbols added in glibc 2.38 (C23 strtol family).
// Declared weak so real glibc definitions take precedence on modern systems;
// only fills the gap on hosts with glibc < 2.38.
#include <stdlib.h>
__attribute__((weak)) long long __isoc23_strtoll(const char *nptr, char **endptr, int base) { return strtoll(nptr, endptr, base); }
__attribute__((weak)) long __isoc23_strtol(const char *nptr, char **endptr, int base)  { return strtol(nptr, endptr, base); }
__attribute__((weak)) unsigned long long __isoc23_strtoull(const char *nptr, char **endptr, int base) { return strtoull(nptr, endptr, base); }
__attribute__((weak)) unsigned long __isoc23_strtoul(const char *nptr, char **endptr, int base) { return strtoul(nptr, endptr, base); }
__attribute__((weak)) float __isoc23_strtof(const char *nptr, char **endptr) { return strtof(nptr, endptr); }
__attribute__((weak)) double __isoc23_strtod(const char *nptr, char **endptr) { return strtod(nptr, endptr); }
