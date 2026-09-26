// The app navigates this window to the embedded server once it is bound.
// If the server fails to start, the app calls this with the error.
window.__opFailed = function (msg) {
  var el = document.getElementById("err");
  el.textContent = msg;
  el.hidden = false;
};
