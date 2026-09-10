/**
 * The 404 page.
 *
 * GitHub Pages serves this for any path it does not have a file for, which for
 * a one-page site means every path but one. A share link that lost its way --
 * a truncated URL, an old path -- should still land on the engine rather than
 * on a dead end, so it forwards, carrying the query string with it.
 */
const home = import.meta.env.BASE_URL;

const link = document.getElementById("home") as HTMLAnchorElement | null;
if (link) link.href = home;

// Only if we are not already there. A base path that itself 404s would
// otherwise redirect to itself forever, and a broken deploy should look
// broken rather than hang the tab.
if (location.pathname !== home && location.pathname !== `${home}index.html`) {
  location.replace(home + location.search + location.hash);
}
