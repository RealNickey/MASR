export function isProductionLike(): boolean {
  return !import.meta.env.DEV || localStorage.getItem("thegai_dev_simulate_prod") === "true";
}
