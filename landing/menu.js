const menu = document.querySelector('.menu-toggle');
const nav = document.querySelector('#main-nav');
const compact = window.matchMedia('(max-width: 740px)');

function setOpen(open) {
  menu.setAttribute('aria-expanded', String(open));
  nav.hidden = compact.matches && !open;
}

function syncViewport() {
  const restoreFocus = (compact.matches && nav.contains(document.activeElement))
    || (!compact.matches && document.activeElement === menu);
  menu.hidden = !compact.matches;
  setOpen(false);
  if (restoreFocus) (compact.matches ? menu : nav.querySelector('a')).focus();
}

syncViewport();
compact.addEventListener('change', syncViewport);
menu.addEventListener('click', () => setOpen(menu.getAttribute('aria-expanded') !== 'true'));
nav.addEventListener('click', (event) => {
  const link = event.target.closest('a[href^="#"]');
  if (!link || !compact.matches) return;
  const section = document.getElementById(link.hash.slice(1));
  if (section) {
    section.tabIndex = -1;
    section.focus({ preventScroll: true });
  }
  setOpen(false);
});
document.querySelector('.site-header').addEventListener('keydown', (event) => {
  if (event.key !== 'Escape' || menu.getAttribute('aria-expanded') !== 'true') return;
  setOpen(false);
  menu.focus();
});
