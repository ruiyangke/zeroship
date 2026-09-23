import type { MarketId } from "@gather/meal-kit/catalog";
export type RecipeSeed = {
  id: string;
  name: string;
  subtitle: string;
  category: "classic" | "vegetarian" | "quick";
  tag: string;
  minutes: number;
  calories: number;
  protein: number;
  premium: number;
  image: string;
  allergens: string[];
  ingredients: string[];
  steps: string[];
  equipment: string;
};

export const recipes: RecipeSeed[] = [
  {
    id: "lemon-chicken",
    name: /* i18n */ "Lemon & herb chicken",
    subtitle: /* i18n */ "with golden potatoes & broccolini",
    category: "classic",
    tag: /* i18n */ "A little comfort",
    minutes: 30,
    calories: 640,
    protein: 42,
    premium: 0,
    image: "chicken",
    allergens: ["milk"],
    equipment: /* i18n */ "Oven, baking tray, frying pan",
    ingredients: [
      /* i18n */ "Chicken breast",
      /* i18n */ "Baby potatoes",
      /* i18n */ "Broccolini",
      /* i18n */ "Lemon, garlic & herb butter",
    ],
    steps: [
      /* i18n */ "Heat the oven to 200°C. Halve the potatoes and toss with oil, salt and pepper on a baking tray.",
      /* i18n */ "Roast the potatoes until golden and tender. Season the chicken with the supplied herbs.",
      /* i18n */ "Sear the chicken, then finish in the oven until cooked through. Check the thickest part reaches a safe cooking temperature.",
      /* i18n */ "Sauté the broccolini. Rest and slice the chicken, then serve with lemon and herb butter.",
    ],
  },
  {
    id: "pesto-pasta",
    name: /* i18n */ "Garden pesto rigatoni",
    subtitle: /* i18n */ "with roasted tomatoes & parmesan",
    category: "vegetarian",
    tag: /* i18n */ "Plant-forward favorite",
    minutes: 20,
    calories: 590,
    protein: 21,
    premium: 0,
    image: "pasta",
    allergens: ["milk", "wheat", "nuts"],
    equipment: /* i18n */ "Saucepan, frying pan",
    ingredients: [
      /* i18n */ "Rigatoni",
      /* i18n */ "Cherry tomatoes",
      /* i18n */ "Basil pesto",
      /* i18n */ "Parmesan & fresh basil",
    ],
    steps: [
      /* i18n */ "Bring salted water to a boil and cook the rigatoni until just tender.",
      /* i18n */ "Sauté the cherry tomatoes in a little oil until blistered.",
      /* i18n */ "Reserve some pasta water, then drain. Toss pasta with pesto and tomatoes, loosening with cooking water.",
      /* i18n */ "Finish with parmesan and torn basil. Taste and season to your liking.",
    ],
  },
  {
    id: "miso-salmon",
    name: /* i18n */ "Miso-glazed salmon",
    subtitle: /* i18n */ "with jasmine rice & crunchy greens",
    category: "quick",
    tag: /* i18n */ "Something special",
    minutes: 25,
    calories: 620,
    protein: 38,
    premium: 250,
    image: "salmon",
    allergens: ["fish", "soy", "sesame"],
    equipment: /* i18n */ "Frying pan, saucepan",
    ingredients: [
      /* i18n */ "Salmon fillet",
      /* i18n */ "Jasmine rice",
      /* i18n */ "Cucumber & edamame",
      /* i18n */ "Miso glaze & sesame",
    ],
    steps: [
      /* i18n */ "Rinse the rice and cook according to the supplied recipe card.",
      /* i18n */ "Brush salmon with miso glaze and pan-cook until cooked through.",
      /* i18n */ "Slice cucumber into ribbons. Warm the edamame.",
      /* i18n */ "Serve salmon over rice with greens and the remaining sauce. Sprinkle with sesame.",
    ],
  },
  {
    id: "harissa-bowl",
    name: /* i18n */ "Sunshine harvest bowl",
    subtitle: /* i18n */ "with harissa cauliflower & chickpeas",
    category: "vegetarian",
    tag: /* i18n */ "Feel-good food",
    minutes: 30,
    calories: 540,
    protein: 19,
    premium: 0,
    image: "bowl",
    allergens: ["milk", "wheat"],
    equipment: /* i18n */ "Oven, baking tray, bowl",
    ingredients: [
      /* i18n */ "Cauliflower",
      /* i18n */ "Chickpeas & couscous",
      /* i18n */ "Harissa spice blend",
      /* i18n */ "Yogurt & herbs",
    ],
    steps: [
      /* i18n */ "Heat the oven to 200°C. Toss cauliflower and chickpeas with oil and harissa.",
      /* i18n */ "Roast until the cauliflower is tender with crisp edges.",
      /* i18n */ "Cover couscous with hot water, let stand, then fluff with a fork.",
      /* i18n */ "Build your bowl and finish with yogurt, lemon and herbs.",
    ],
  },
  {
    id: "mushroom-orzo",
    name: /* i18n */ "Creamy woodland orzo",
    subtitle: /* i18n */ "with baby spinach & fresh thyme",
    category: "vegetarian",
    tag: /* i18n */ "Cozy nights in",
    minutes: 25,
    calories: 570,
    protein: 22,
    premium: 0,
    image: "mushroom",
    allergens: ["milk", "wheat"],
    equipment: /* i18n */ "Deep frying pan",
    ingredients: [
      /* i18n */ "Orzo pasta",
      /* i18n */ "Mixed mushrooms",
      /* i18n */ "Baby spinach",
      /* i18n */ "Parmesan, cream & thyme",
    ],
    steps: [
      /* i18n */ "Slice the mushrooms. Sauté in oil until golden, then add the thyme.",
      /* i18n */ "Stir in the orzo and supplied stock. Simmer, stirring, until tender.",
      /* i18n */ "Fold in spinach and cream, letting the spinach wilt.",
      /* i18n */ "Finish with parmesan, black pepper and a little fresh thyme.",
    ],
  },
  {
    id: "sesame-tofu",
    name: /* i18n */ "Sticky ginger tofu",
    subtitle: /* i18n */ "with rainbow vegetables & rice",
    category: "quick",
    tag: /* i18n */ "Big flavor, little effort",
    minutes: 20,
    calories: 510,
    protein: 26,
    premium: 0,
    image: "tofu",
    allergens: ["soy", "sesame", "wheat"],
    equipment: /* i18n */ "Frying pan, saucepan",
    ingredients: [
      /* i18n */ "Firm tofu",
      /* i18n */ "Rice",
      /* i18n */ "Broccoli & carrots",
      /* i18n */ "Ginger sesame sauce",
    ],
    steps: [
      /* i18n */ "Rinse and cook the rice. Pat the tofu dry and cut into cubes.",
      /* i18n */ "Pan-fry tofu in a little oil until crisp on all sides.",
      /* i18n */ "Add the vegetables and stir-fry until tender. Add the ginger sesame sauce.",
      /* i18n */ "Toss gently until glossy, then serve over rice.",
    ],
  },
];

export const marketOfferings: Record<MarketId, Record<string, number>> = {
  us: {
    "lemon-chicken": 0,
    "pesto-pasta": 0,
    "miso-salmon": 250,
    "harissa-bowl": 0,
    "mushroom-orzo": 0,
    "sesame-tofu": 0,
  },
  uk: {
    "lemon-chicken": 0,
    "pesto-pasta": 0,
    "miso-salmon": 300,
    "harissa-bowl": 0,
    "mushroom-orzo": 0,
  },
  cn: {
    "lemon-chicken": 0,
    "miso-salmon": 1000,
    "mushroom-orzo": 0,
    "sesame-tofu": 0,
  },
};
export function recipesForMarket(market: MarketId): RecipeSeed[] {
  return recipes
    .filter((recipe) => Object.hasOwn(marketOfferings[market], recipe.id))
    .map((recipe) => ({
      ...recipe,
      premium: marketOfferings[market][recipe.id],
    }));
}

export function seedRecipeDraft(
  recipe: RecipeSeed,
  translate: (text: string) => string,
) {
  const pantry = /* i18n */ "Cooking oil, salt and pepper";
  const storage =
    /* i18n */ "Follow the storage instructions and use-by date on each ingredient label.";
  const nutritionBasis =
    /* i18n */ "Illustrative values per serving, prepared as directed.";
  const quantities = {
    "lemon-chicken": [300, 400, 200, 80],
    "pesto-pasta": [180, 250, 80, 40],
    "miso-salmon": [280, 150, 200, 60],
    "harissa-bowl": [300, 250, 20, 100],
    "mushroom-orzo": [180, 250, 100, 120],
    "sesame-tofu": [300, 150, 250, 80],
  }[recipe.id]!;
  const { id, premium, ...content } = recipe;
  return {
    ...content,
    pantry,
    storage,
    nutritionBasis,
    baseServings: 2,
    image: recipe.image as
      | "chicken"
      | "pasta"
      | "salmon"
      | "bowl"
      | "mushroom"
      | "tofu",
    allergens: recipe.allergens as (
      | "milk"
      | "wheat"
      | "nuts"
      | "fish"
      | "soy"
      | "sesame"
    )[],
    quantities: quantities.map((amount) => ({ amount, unit: "g" as const })),
    translations: {
      zh: {
        name: translate(recipe.name),
        subtitle: translate(recipe.subtitle),
        tag: translate(recipe.tag),
        equipment: translate(recipe.equipment),
        ingredients: recipe.ingredients.map(translate),
        steps: recipe.steps.map(translate),
        pantry: translate(pantry),
        storage: translate(storage),
        nutritionBasis: translate(nutritionBasis),
      },
    },
  };
}
