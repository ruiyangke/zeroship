const app = document.querySelector<HTMLDivElement>("#app");

if (app) {
  app.textContent =
    "storage-gallery exercises @zeroship/storage (env.storage). " +
    "Call the RPC procedures (gallery.put / gallery.get / gallery.list / gallery.delete) " +
    "to put, read back, enumerate, and remove objects.";
}
