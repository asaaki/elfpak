/* Integration fixture: a shared library installed where the loader does not
 * look. Nothing in /opt is a default search directory, and the program that
 * links against it declares no DT_RPATH, so on a normal system it is only
 * findable because `ldconfig` recorded it in /etc/ld.so.cache.
 *
 * A bundle has no `ldconfig`, which is what makes this the interesting case:
 * the packaged application can only start if elfpak wrote a cache of its own. */

int vendor_value(void) { return 7; }
