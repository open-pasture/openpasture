// The app navigates this window to the embedded server once it is bound.
// If the server fails to start, the app calls this with the error.
window.__opFailed = function (msg) {
  document.getElementById("wait").hidden = true;
  var el = document.getElementById("err");
  el.textContent = msg;
  el.hidden = false;
};

// While another openpasture opening this folder upgrades its database, the app says so here.
window.__opWaiting = function (msg) {
  var el = document.getElementById("wait");
  el.textContent = msg;
  el.hidden = !msg;
};
