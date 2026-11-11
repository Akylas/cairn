export function mb(bytes) {
  if (bytes == null) return "—";
  return `${(bytes / 1048576).toFixed(1)} MB`;
}

export function pct(value) {
  if (value == null) return "—";
  const sign = value > 0 ? "+" : "";
  return `${sign}${value.toFixed(1)}%`;
}

export const KIND_LABEL = {
  basemap: "Basemap",
  routes: "Routes",
  terrain_rgb: "Terrain RGB",
  contours: "Contours",
  // a legacy artifact kind, not a build step: older archives are named `_hillshade`
  hillshade: "Hillshade",
  valhalla_package: "Valhalla",
  unknown: "Unknown",
};

export function formatLabel(format) {
  if (typeof format === "string") return format.toUpperCase();
  if (format && format.other) return format.other;
  return "—";
}
