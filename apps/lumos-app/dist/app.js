const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const queryEl = document.getElementById("query");
const resultsEl = document.getElementById("results");

let results = [];
let selected = 0;
let debounceTimer = null;

const EXT_STYLE = {
  js: ["JS", "#e6b400"], jsx: ["JSX", "#e6b400"], ts: ["TS", "#3178c6"], tsx: ["TSX", "#3178c6"],
  vue: ["V", "#41b883"], json: ["{ }", "#8bc34a"], md: ["MD", "#8b8b93"],
  yml: ["YML", "#a970ff"], yaml: ["YML", "#a970ff"], css: ["CSS", "#5b9dd9"], scss: ["SCSS", "#cc6699"],
  html: ["</>", "#e8734a"], rs: ["RS", "#b7743a"], toml: ["TOML", "#a970ff"],
  cs: ["C#", "#8c5ad1"], csproj: ["C#", "#8c5ad1"], sln: ["SLN", "#8c5ad1"],
  php: ["PHP", "#6f7cba"], py: ["PY", "#3776ab"], ini: ["INI", "#8b8b93"], config: ["CFG", "#8b8b93"],
  txt: ["TXT", "#6b6b73"], sql: ["SQL", "#4a90a4"],
};

function iconFor(ext) {
  const key = (ext || "").toLowerCase();
  return EXT_STYLE[key] || [(key || "?").slice(0, 4).toUpperCase(), "#5a5a63"];
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// Rebuilds the DOM — only when the result *set* changes (new search, or the
// overlay was just reopened). Moving the selection must never call this: a
// full innerHTML replace re-triggers every row's entrance animation at once,
// which is what "flicker" actually was.
function renderResults() {
  if (results.length === 0) {
    const hasQuery = queryEl.value.trim().length > 0;
    resultsEl.innerHTML = `
      <div id="empty-state">
        <div class="big">${hasQuery ? "∅" : "◔"}</div>
        <div>${hasQuery ? "No matches" : "Start typing to search your files"}</div>
      </div>`;
    return;
  }

  resultsEl.innerHTML = results
    .map((r, i) => {
      const [label, color] = iconFor(r.extension);
      const project = r.project_name ? `<span class="badge">${escapeHtml(r.project_name)}</span>` : "";
      return `
        <div class="result ${i === selected ? "selected" : ""}" data-index="${i}">
          <div class="file-icon" style="background:${color}">${label}</div>
          <div class="result-text">
            <div class="result-name">${escapeHtml(r.filename)}</div>
            <div class="result-path">${escapeHtml(r.path)}</div>
          </div>
          <div class="result-meta">
            ${project}
            <span class="size">${formatSize(r.size)}</span>
          </div>
        </div>`;
    })
    .join("");

  scrollSelectedIntoView();
}

// Just moves the highlight — toggles a class on existing nodes so the CSS
// transition can animate it smoothly instead of the whole list flashing in.
function updateSelection() {
  resultsEl.querySelectorAll(".result").forEach((el) => {
    el.classList.toggle("selected", Number(el.dataset.index) === selected);
  });
  scrollSelectedIntoView();
}

function scrollSelectedIntoView() {
  const selectedEl = resultsEl.querySelector(".result.selected");
  if (selectedEl) selectedEl.scrollIntoView({ block: "nearest" });
}

function escapeHtml(s) {
  return (s || "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

async function runSearch(query) {
  if (!query.trim()) {
    results = [];
    selected = 0;
    renderResults();
    return;
  }
  try {
    results = await invoke("search_files", { query });
    selected = 0;
    renderResults();
  } catch (e) {
    console.error("search failed", e);
  }
}

// The Rust side handles whether the overlay hides or stays put — it drops
// always-on-top and waits briefly to see if focus bounces back (dialog
// cancelled) or moves on for good (file opened) before deciding.
function activate(r, mode) {
  if (!r) return;
  if (mode === "reveal") {
    invoke("reveal_in_explorer", { path: r.path });
  } else {
    invoke("open_file", { path: r.path });
  }
}

queryEl.addEventListener("input", () => {
  clearTimeout(debounceTimer);
  const q = queryEl.value;
  debounceTimer = setTimeout(() => runSearch(q), 120);
});

// Attached to document, not just the input: clicking a result row shouldn't
// be able to strand keyboard navigation by moving focus off the input.
document.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") {
    e.preventDefault();
    if (results.length) {
      selected = (selected + 1) % results.length;
      updateSelection();
    }
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    if (results.length) {
      selected = (selected - 1 + results.length) % results.length;
      updateSelection();
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    invoke("hide_overlay");
  } else if (e.key === "Enter") {
    e.preventDefault();
    activate(results[selected], e.ctrlKey ? "reveal" : "open");
  }
});

resultsEl.addEventListener("mouseover", (e) => {
  const row = e.target.closest(".result");
  if (!row) return;
  const idx = Number(row.dataset.index);
  if (idx !== selected) {
    selected = idx;
    updateSelection();
  }
});

resultsEl.addEventListener("click", (e) => {
  const row = e.target.closest(".result");
  if (!row) return;
  const idx = Number(row.dataset.index);
  activate(results[idx], e.ctrlKey ? "reveal" : "open");
});

listen("overlay-shown", () => {
  queryEl.value = "";
  results = [];
  selected = 0;
  renderResults();
  queryEl.focus();
});

renderResults();
