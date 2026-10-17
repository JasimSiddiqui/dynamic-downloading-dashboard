(function () {
  var saved = null;
  try {
    saved = localStorage.getItem("ddd-theme");
  } catch (e) {}
  var theme = saved || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  document.documentElement.setAttribute("data-theme", theme);
})();
