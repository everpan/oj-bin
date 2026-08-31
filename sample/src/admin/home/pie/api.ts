function get(): void {
  json.ok([
    { value: 42, code: "electronics" },
    { value: 25, code: "home_goods" },
    { value: 18, code: "apparel_accessories" },
    { value: 60, code: "food_beverages" },
    { value: 33, code: "beauty_skincare" },
  ]);
}
get.route = "/home/pie";
export default { get };
