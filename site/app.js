const root = document.documentElement;
// Preserve existing visitor preferences on the unchanged site origin.
const STORAGE = { theme: 'disktop-theme', lang: 'disktop-lang' };

function writeStored(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    // Storage may be unavailable; the choice then lasts for this page view only.
  }
}

// English is the source text in index.html; German replaces it at runtime.
const UI = {
  en: {
    title: 'spctop — your space, in view',
    description: "spctop is a Linux terminal space monitor. Watch used and free space, disk fill history and directory growth. Open the separate I/O view for disk activity and processes.",
    copyCommands: 'Copy commands',
    copied: 'Copied!',
    pressCopy: 'Press Ctrl/Cmd+C',
    copiedStatus: 'Installation commands copied to the clipboard.',
    selectedStatus: 'Commands selected. Press Control C or Command C to copy.',
    toLight: 'Switch to light theme',
    toDark: 'Switch to dark theme',
    lightOn: 'Light theme on.',
    darkOn: 'Dark theme on.',
    languageOn: 'English language on.',
    otherLanguage: { code: 'DE', lang: 'de', label: 'Auf Deutsch umschalten' },
  },
  de: {
    title: 'spctop — dein Speicherplatz im Blick',
    description: "spctop ist ein Speichermonitor für das Linux-Terminal. Belegter und freier Platz, Füllstandsverlauf und Verzeichniswachstum. Datenträgeraktivität und Prozesse stehen in der separaten I/O-Ansicht.",
    copyCommands: 'Befehle kopieren',
    copied: 'Kopiert!',
    pressCopy: 'Strg/Cmd+C drücken',
    copiedStatus: 'Installationsbefehle in die Zwischenablage kopiert.',
    selectedStatus: 'Befehle markiert. Zum Kopieren Strg+C oder Cmd+C drücken.',
    toLight: 'Zum hellen Design wechseln',
    toDark: 'Zum dunklen Design wechseln',
    lightOn: 'Helles Design aktiv.',
    darkOn: 'Dunkles Design aktiv.',
    languageOn: 'Deutsche Sprache aktiv.',
    otherLanguage: { code: 'EN', lang: 'en', label: 'Switch to English' },
  },
};

const DE = {
  skip: 'Zum Inhalt springen',
  homeLabel: 'spctop Startseite',
  navLabel: 'Hauptnavigation',
  navInside: 'Blick in spctop',
  navInstall: 'Installation',
  intro: '<span class="status-dot" aria-hidden="true"></span>Zu Hause in deinem Terminal.',
  heroTitle: "Dein Speicher.<br>Klar im Blick.",
  heroDescription: "Behalte belegten und freien Platz auf deinen Datenträgern im Blick. Verfolge Füllstände im Zeitverlauf und finde wachsende Verzeichnisse. Ein Speichermonitor für das Linux-Terminal mit dem vertrauten Gefühl von htop und nvtop.",
  getSpctop: 'spctop holen <span aria-hidden="true">↗</span>',
  closerLook: 'Genauer hinsehen',
  heroFacts: 'Gebaut in Rust <span aria-hidden="true">/</span> Gemacht für Linux <span aria-hidden="true">/</span> GPLv3-Lizenz',
  read: '<i class="rd-dot" aria-hidden="true"></i>Lesen',
  write: '<i class="wr-dot" aria-hidden="true"></i>Schreiben',
  util: '<i class="io-dot" aria-hidden="true"></i>IO',
  fill: '<i class="us-dot" aria-hidden="true"></i>Füllstand',
  compactView: 'Jede Platte, eine Ansicht.',
  heroAlt: "Echte spctop-Speicheransicht mit Füllständen und Verlauf über der Dateisystemtabelle",
  demoCaption: 'Echte Terminalausgabe. Demodaten.',
  watchRun: 'Im Einsatz ansehen <span aria-hidden="true">▷</span>',
  capabilitiesLabel: 'Überwachungsfunktionen',
  signal1Title: "Wissen, was voll ist.",
  signal1Text: "Belegter und freier Platz, mit Füllstandsverlauf pro Datenträger.",
  signal2Title: "Wachstum verfolgen.",
  signal2Text: "Verzeichnisgrößen und Änderungen zwischen Scans.",
  signal3Title: "I/O bei Bedarf prüfen.",
  signal3Text: "Datenträgeraktivität und Prozessraten in einer eigenen Ansicht.",
  insideTitle: "Jeder Datenträger.<br>Platz im Blick.",
  insideText: "spctop startet mit dem Speicherplatz: Füllstände, Verlauf und Dateisysteme. In Home verfolgst du wachsende Verzeichnisse, in der I/O-Ansicht prüfst du Lese- und Schreibraten sowie Prozesse. spctop passt sich bis hinunter zu einem Terminal mit 36 × 16 Zeichen an.",
  tablistLabel: 'spctop-Ansichten erkunden',
  'tab-disks-title': "Speicherplatz",
  'tab-disks-text': "Belegt, frei und Füllstandsverlauf. Dateisysteme darunter.",
  'tab-home-title': "Wachstum in Home",
  'tab-home-text': "Verzeichnisgrößen und Änderungen zwischen Scans.",
  'tab-io-title': "I/O-Details",
  'tab-io-text': "Lesen und Schreiben in der separaten Ansicht.",
  explorerHint: 'Wähle eine Ansicht, um die echte Oberfläche zu sehen.',
  shortcut1: '<kbd>F5</kbd> Einen Datenträger fokussieren',
  shortcut2: "<kbd>F4</kbd> Speicher, Home oder I/O wählen",
  shortcut3: '<kbd>/</kbd> Tabelle durchsuchen',
  shortcut4: '<kbd>F12</kbd> Einstellungen speichern',
  disksAlt: "spctop im DEMO-Modus mit Speicherplatz, Füllstandsverlauf und Dateisystembelegung",
  ioAlt: "Echte sekundäre I/O-Ansicht mit Lese- und Schreibanzeigen, Aktivitätsgraphen und Prozess-I/O",
  homeAlt: "Echte Home-Ansicht mit Verzeichnisgrößen und Wachstum seit dem vorigen und ersten Scan",
  screenCaption: 'Native Terminalfarben. Englisch oder Deutsch. Dein Theme kommt mit.',
  installTitle: 'Einmal einrichten.<br>Dann einfach spctop.',
  installText: 'Aus dem Quellcode bauen und in dein Benutzerverzeichnis installieren. spctop läuft sofort als normaler Benutzer; der optionale Helfer ergänzt die Prozesse aller Benutzer und die Verzeichnisgrößen unter freigegebenen Wurzeln (Standard <code>/home</code>). So oder so startest du <code>spctop</code> ohne sudo.',
  requirements: 'Du brauchst Linux und Rust 1.88+. Die Befehle hier gelten für Ubuntu 24.04 oder neuer.',
  adminSummary: 'Was macht der optionale Helfer?',
  adminText: 'Ohne ihn zeigt spctop deine eigenen Prozesse und die Verzeichnisse, die du lesen darfst, und sagt, wie viele Prozesse verborgen sind. Die Einrichtung des Helfers erfordert einmalig eine Authentifizierung als Administrator und installiert einen root-eigenen Collector mit nur <code>cap_dac_read_search</code> und <code>cap_sys_ptrace</code>. Mitglieder deiner primären Gruppe sehen dann das Speicher-I/O aller Prozesse und die Namen und Größen der Verzeichnisse unter den Scan-Wurzeln, die ein Administrator in <code>/etc/spctop/scan-roots</code> einträgt (Standard <code>/home</code>), nie Dateinamen oder Inhalte. Eine geteilte primäre Gruppe lehnt die Einrichtung ohne ausdrückliche Zustimmung ab, und wer sich als Administrator authentifiziert, muss deinem Checkout vertrauen, denn root installiert den von dir gebauten Helfer. Die Terminal-Oberfläche selbst erhält nie Rechte.',
  helperLink: 'Lies, wie der Helfer funktioniert',
  accessNote: 'Open Source unter der GNU GPLv3 oder neuer. Zum Klonen ist kein GitHub-Konto nötig.',
  installHeading: 'Aus dem Quellcode installieren',
  copyCommands: 'Befehle kopieren',
  commentHelper: '# Optional, einmalig: setcap-Werkzeuge + Helfer',
  commentNoSudo: '# Ab jetzt ohne sudo',
  demoText: 'Eine gekennzeichnete Vorschau mit synthetischen Daten.',
  detailsTitle: 'Echte Zähler.<br>Klare Grenzen.',
  detailsText1: "Belegter und freier Platz stammen aus eingehängten Dateisystemen, über Partitionen, LUKS, LVM und MD ihren Datenträgern zugeordnet. Verzeichnisscans bleiben auf einem Dateisystem und verfolgen Wachstum innerhalb der Sitzung, ohne Dateiinhalte zu lesen.",
  detailsText2: "Unvollständige Scans und nicht verfügbare Daten bleiben sichtbar. Die separate I/O-Ansicht nutzt Datenträger- und Prozesszähler des Kernels; Prozess-I/O lässt sich keinem Gerät zuordnen, und zwischen Aktualisierungen beendete Prozesse erscheinen nicht.",
  measurementsLink: 'Die Messwerte verstehen',
  videoLabel: "Echte spctop-DEMO-Aufnahme von Speicher, Wachstum in Home und dem I/O-Untermenü",
  videoCaption: "Speicherübersicht, Wachstum in Home und das I/O-Untermenü in einer echten Terminalaufnahme. Synthetische DEMO-Daten.",
  closingTitle: "Ein kleines Fenster<br>in deinen Speicherplatz.",
  makeRoom: 'Platz für spctop schaffen <span aria-hidden="true">↗</span>',
  topLabel: 'Zurück nach oben',
  madeBy: 'Gemacht von <a href="https://itmitalles.de">itmitalles</a>. Inspiriert von <a href="https://github.com/htop-dev/htop">htop</a> und <a href="https://github.com/Syllo/nvtop">nvtop</a>. Das Gegenstück für Datenträger zu <a href="https://nettop.wutz.io">nwtop</a>.',
  sourceDocs: 'Quellcode &amp; Dokumentation',
};

const textNodes = [...document.querySelectorAll('[data-i18n]')].map(element => ({
  element, key: element.dataset.i18n, en: element.innerHTML,
}));
const attrNodes = [...document.querySelectorAll('[data-i18n-attr]')].flatMap(element =>
  element.dataset.i18nAttr.split(';').map(pair => {
    const [attr, key] = pair.split(':');
    return { element, attr, key, en: element.getAttribute(attr) };
  })
);
const descriptionMeta = document.querySelector('meta[name="description"]');
const languageToggle = document.getElementById('language-toggle');
const themeToggle = document.getElementById('theme-toggle');
const statusRegion = document.getElementById('copy-status');
let currentLang = root.dataset.lang === 'de' ? 'de' : 'en';

const t = key => UI[currentLang][key];

function applyLanguage(lang) {
  currentLang = lang;
  root.lang = lang;
  root.dataset.lang = lang;
  for (const node of textNodes) {
    node.element.innerHTML = (lang === 'de' && DE[node.key]) || node.en;
  }
  for (const node of attrNodes) {
    node.element.setAttribute(node.attr, (lang === 'de' && DE[node.key]) || node.en);
  }
  document.title = t('title');
  descriptionMeta.setAttribute('content', t('description'));
  const other = t('otherLanguage');
  languageToggle.textContent = other.code;
  languageToggle.lang = other.lang;
  languageToggle.setAttribute('aria-label', other.label);
  languageToggle.title = other.label;
  updateThemeControl();
  root.classList.remove('i18n-pending');
}

languageToggle.addEventListener('click', () => {
  const lang = currentLang === 'de' ? 'en' : 'de';
  writeStored(STORAGE.lang, lang);
  applyLanguage(lang);
  statusRegion.textContent = t('languageOn');
});

const systemLight = window.matchMedia('(prefers-color-scheme: light)');
const themeMetas = [...document.querySelectorAll('meta[name="theme-color"]')].map(meta => ({
  meta, content: meta.getAttribute('content'),
}));
const THEME_COLORS = { dark: '#0b1725', light: '#f4f7f2' };

const systemTheme = () => (systemLight.matches ? 'light' : 'dark');
const effectiveTheme = () => root.dataset.theme || systemTheme();

function updateThemeControl() {
  const label = effectiveTheme() === 'dark' ? t('toLight') : t('toDark');
  themeToggle.setAttribute('aria-label', label);
  themeToggle.title = label;
  for (const { meta, content } of themeMetas) {
    meta.setAttribute('content', root.dataset.theme ? THEME_COLORS[root.dataset.theme] : content);
  }
}

// The toggle always flips the visible theme. Picking the system theme again
// removes the override, so the page keeps following the operating system.
themeToggle.addEventListener('click', () => {
  const theme = effectiveTheme() === 'dark' ? 'light' : 'dark';
  if (theme === systemTheme()) {
    delete root.dataset.theme;
    writeStored(STORAGE.theme, null);
  } else {
    root.dataset.theme = theme;
    writeStored(STORAGE.theme, theme);
  }
  updateThemeControl();
  statusRegion.textContent = theme === 'light' ? t('lightOn') : t('darkOn');
});
systemLight.addEventListener('change', updateThemeControl);

applyLanguage(currentLang);

const tabs = [...document.querySelectorAll('[data-view]')];
const mobileTabs = window.matchMedia('(max-width: 800px)');

function updateTabOrientation() {
  document.querySelector('[role="tablist"]').setAttribute(
    'aria-orientation', mobileTabs.matches ? 'horizontal' : 'vertical'
  );
}

updateTabOrientation();
mobileTabs.addEventListener('change', updateTabOrientation);

function selectView(tab, moveFocus = false) {
  for (const candidate of tabs) {
    const selected = candidate === tab;
    candidate.setAttribute('aria-selected', String(selected));
    candidate.tabIndex = selected ? 0 : -1;
    document.getElementById(candidate.getAttribute('aria-controls')).hidden = !selected;
  }
  if (moveFocus) tab.focus();
}

for (const tab of tabs) {
  tab.addEventListener('click', () => selectView(tab));
  tab.addEventListener('keydown', event => {
    const index = tabs.indexOf(tab);
    let next;
    if (event.key === 'ArrowRight' || event.key === 'ArrowDown') next = (index + 1) % tabs.length;
    if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') next = (index + tabs.length - 1) % tabs.length;
    if (event.key === 'Home') next = 0;
    if (event.key === 'End') next = tabs.length - 1;
    if (next !== undefined) {
      event.preventDefault();
      selectView(tabs[next], true);
    }
  });
}

for (const button of document.querySelectorAll('[data-copy]')) {
  let reset;
  button.addEventListener('click', async () => {
    const source = document.getElementById(button.dataset.copy);
    const status = document.getElementById('copy-status');
    clearTimeout(reset);
    try {
      await navigator.clipboard.writeText(source.textContent.trim());
      button.textContent = t('copied');
      status.textContent = t('copiedStatus');
    } catch {
      const range = document.createRange();
      range.selectNodeContents(source);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      button.textContent = t('pressCopy');
      status.textContent = t('selectedStatus');
    }
    reset = setTimeout(() => { button.textContent = t('copyCommands'); }, 3500);
  });
}
