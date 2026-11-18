// Runs before first paint (no flash): applies a saved theme choice.
(function () {
  var d = document.documentElement;
  d.classList.remove("no-js");
  try {
    var t = localStorage.getItem("bf-theme");
    if (t === "light" || t === "dark") d.setAttribute("data-theme", t);
  } catch (e) {}
})();
