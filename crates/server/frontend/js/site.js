(() => {
  if (!navigator.clipboard?.writeText) {
    return;
  }

  for (const control of document.querySelectorAll("[data-copy-lightning-address]")) {
    control.hidden = false;
    control.addEventListener("click", async () => {
      const address = control.dataset.copyLightningAddress;
      if (!address) {
        return;
      }

      const originalLabel = control.textContent;
      try {
        await navigator.clipboard.writeText(address);
        control.textContent = "Copied";
      } catch {
        control.textContent = "Copy failed";
      }
      window.setTimeout(() => {
        control.textContent = originalLabel;
      }, 1500);
    });
  }
})();
