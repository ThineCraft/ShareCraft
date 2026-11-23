
document.documentElement.classList.add('js');
const toggle = document.querySelector('.menu-toggle');
const navigation = document.querySelector('#project-navigation');
if (toggle && navigation) {
  toggle.addEventListener('click', () => {
    const expanded = toggle.getAttribute('aria-expanded') === 'true';
    toggle.setAttribute('aria-expanded', String(!expanded));
    navigation.dataset.open = String(!expanded);
  });
}

// Click a screenshot to see the whole picture, fitted to the window.
const shots = document.querySelectorAll('.shot img');
if (shots.length && typeof HTMLDialogElement === 'function') {
  const viewer = document.createElement('dialog');
  viewer.className = 'viewer';
  viewer.setAttribute('aria-label', 'Screenshot, full size');
  viewer.innerHTML = '<button class="viewer-close" type="button" aria-label="Close">×</button><img alt="">';
  document.body.append(viewer);
  const big = viewer.querySelector('img');
  const open = (img) => {
    big.src = img.currentSrc || img.src;
    big.alt = img.alt;
    viewer.showModal();
  };
  viewer.addEventListener('click', () => viewer.close());
  shots.forEach((img) => {
    img.tabIndex = 0;
    img.setAttribute('role', 'button');
    img.setAttribute('aria-label', 'View full size: ' + img.alt);
    img.addEventListener('click', () => open(img));
    img.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(img); }
    });
  });
}
