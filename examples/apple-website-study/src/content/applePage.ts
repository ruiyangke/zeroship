import {
  BatteryFull,
  Camera,
  Cpu,
  CreditCard,
  Headphones,
  PackageCheck,
  ShieldCheck,
  ShoppingBag,
  Smartphone,
  Sparkles,
  Truck,
} from "lucide-react";
import { appleAssets } from "../assets/appleAssets";
import type {
  ChapterItem,
  CompanionItem,
  ConsiderCard,
  EssentialCard,
  FooterColumn,
  MegaMenuModel,
  Product,
  VisualCard,
} from "../types";

export const globalNav = [
  "Store",
  "Mac",
  "iPad",
  "iPhone",
  "Watch",
  "Vision",
  "AirPods",
  "TV & Home",
  "Entertainment",
  "Accessories",
  "Support",
] as const;

export type GlobalNavLabel = (typeof globalNav)[number];

export const megaMenus: Record<GlobalNavLabel, MegaMenuModel> = {
  Store: {
    eyebrow: "Store",
    columns: [
      {
        heading: "Shop",
        elevated: true,
        links: [
          "Shop the Latest",
          "Mac",
          "iPad",
          "iPhone",
          "Apple Watch",
          "Apple Vision Pro",
          "AirPods",
          "Accessories",
        ],
      },
      {
        heading: "Quick Links",
        links: ["Find a Store", "Order Status", "Apple Trade In", "Financing", "Personal Setup"],
      },
      {
        heading: "Shop Special Stores",
        links: ["Certified Refurbished", "Education", "Business", "Veterans and Military", "Government"],
      },
    ],
  },
  Mac: {
    eyebrow: "Mac",
    columns: [
      {
        heading: "Explore Mac",
        elevated: true,
        links: [
          "Explore All Mac",
          "MacBook Air",
          "MacBook Pro",
          "iMac",
          "Mac mini",
          "Mac Studio",
          "Displays",
          "Compare Mac",
        ],
      },
      {
        heading: "Shop Mac",
        links: ["Shop Mac", "Mac Accessories", "Personal Setup"],
      },
      {
        heading: "More from Mac",
        links: ["Mac Support", "AppleCare", "macOS", "Continuity", "iCloud+"],
      },
    ],
  },
  iPad: {
    eyebrow: "iPad",
    columns: [
      {
        heading: "Explore iPad",
        elevated: true,
        links: ["Explore All iPad", "iPad Pro", "iPad Air", "iPad", "iPad mini", "Compare iPad"],
      },
      {
        heading: "Shop iPad",
        links: ["Shop iPad", "iPad Accessories", "Apple Pencil", "Keyboards"],
      },
      {
        heading: "More from iPad",
        links: ["iPad Support", "AppleCare", "iPadOS", "Education"],
      },
    ],
  },
  iPhone: {
    eyebrow: "iPhone",
    columns: [
      {
        heading: "Explore iPhone",
        elevated: true,
        links: [
          "Explore All iPhone",
          "iPhone 17 Pro",
          "iPhone Air",
          "iPhone 17",
          "iPhone 17e",
          "iPhone 16",
        ],
        secondaryLinks: ["Compare iPhone", "Switch from Android"],
      },
      {
        heading: "Shop iPhone",
        links: [
          "Shop iPhone",
          "iPhone Accessories",
          "Apple Trade In",
          "Carrier Deals at Apple",
          "Financing",
          "Personal Setup",
        ],
      },
      {
        heading: "More from iPhone",
        links: [
          "iPhone Support",
          "AppleCare",
          "iOS 26",
          "Apple Intelligence",
          "Apps by Apple",
          "iPhone Privacy",
          "Better with Mac",
          "iCloud+",
          "Wallet, Pay, Card",
          "Siri",
        ],
      },
    ],
  },
  Watch: {
    eyebrow: "Watch",
    columns: [
      {
        heading: "Explore Watch",
        elevated: true,
        links: ["Explore All Watch", "Watch Series", "Watch Ultra", "Watch SE", "Compare Watch"],
      },
      {
        heading: "Shop Watch",
        links: ["Shop Watch", "Watch Bands", "Watch Accessories"],
      },
      {
        heading: "More from Watch",
        links: ["Watch Support", "AppleCare", "watchOS", "Fitness"],
      },
    ],
  },
  Vision: {
    eyebrow: "Vision",
    columns: [
      {
        heading: "Explore Vision",
        elevated: true,
        links: ["Explore Vision", "Guided Tour", "Tech Specs", "Compare Displays"],
      },
      {
        heading: "Shop Vision",
        links: ["Shop Vision", "Book a Demo", "Accessories", "Setup"],
      },
      {
        heading: "More from Vision",
        links: ["Vision Support", "AppleCare", "visionOS", "Spatial Apps"],
      },
    ],
  },
  AirPods: {
    eyebrow: "AirPods",
    columns: [
      {
        heading: "Explore AirPods",
        elevated: true,
        links: ["Explore All AirPods", "AirPods Pro", "AirPods Max", "AirPods", "Compare AirPods"],
      },
      {
        heading: "Shop AirPods",
        links: ["Shop AirPods", "AirPods Accessories", "Personalize"],
      },
      {
        heading: "More from AirPods",
        links: ["AirPods Support", "AppleCare", "Audio Sharing", "Hearing Health"],
      },
    ],
  },
  "TV & Home": {
    eyebrow: "TV & Home",
    columns: [
      {
        heading: "Explore TV & Home",
        elevated: true,
        links: ["Explore TV & Home", "TV 4K", "HomePod", "HomePod mini", "Home app"],
      },
      {
        heading: "Shop TV & Home",
        links: ["Shop TV 4K", "Shop HomePod", "Home Accessories"],
      },
      {
        heading: "More from TV & Home",
        links: ["TV Support", "Home Support", "Streaming", "Smart Home"],
      },
    ],
  },
  Entertainment: {
    eyebrow: "Entertainment",
    columns: [
      {
        heading: "Explore Entertainment",
        elevated: true,
        links: ["Apple One", "TV+", "Music", "Arcade", "Fitness+", "News+", "Podcasts", "Books"],
      },
      {
        heading: "Support",
        links: ["TV+ Support", "Music Support", "Subscriptions", "Gift Cards"],
      },
      {
        heading: "More",
        links: ["Originals", "Live Sports", "Family Sharing", "Student Offers"],
      },
    ],
  },
  Accessories: {
    eyebrow: "Accessories",
    columns: [
      {
        heading: "Shop Accessories",
        elevated: true,
        links: ["Shop All Accessories", "Mac", "iPad", "iPhone", "Apple Watch", "AirPods", "Home"],
      },
      {
        heading: "Explore Accessories",
        links: ["Made by Apple", "Cases & Protection", "Charging", "Keyboards"],
      },
      {
        heading: "More",
        links: ["Support", "Compatibility", "New Arrivals", "Gift Ideas"],
      },
    ],
  },
  Support: {
    eyebrow: "Support",
    columns: [
      {
        heading: "Explore Support",
        elevated: true,
        links: ["iPhone", "Mac", "iPad", "Watch", "AirPods", "Music", "TV"],
      },
      {
        heading: "Get Help",
        links: ["Community", "Check Coverage", "Repair", "Contact Us"],
      },
      {
        heading: "Helpful Topics",
        links: ["AppleCare", "Account", "iCloud+", "Accessibility"],
      },
    ],
  },
};

export const chapterItems: ChapterItem[] = [
  { label: "iPhone 17 Pro", image: appleAssets.nav17Pro, status: "" },
  { label: "iPhone Air", image: appleAssets.navAir, status: "" },
  { label: "iPhone 17", image: appleAssets.nav17, status: "" },
  { label: "iPhone 17e", image: appleAssets.nav17e, status: "New" },
  { label: "iPhone 16", image: appleAssets.nav16, status: "" },
  { label: "Compare", image: appleAssets.navCompare, status: "" },
  { label: "Accessories", image: appleAssets.navAccessories, status: "" },
  { label: "Shop iPhone", image: appleAssets.navShop, status: "" },
  { label: "iOS", image: appleAssets.navIos, status: "Preview" },
];

export const products: Product[] = [
  {
    name: "iPhone 17 Pro",
    image: appleAssets.select17Pro,
    alt: "iPhone 17 Pro in cosmic orange.",
    tagline: ["Innovative design for ultimate", "performance and battery life."],
    availability: "",
    price: "From $1099 or $45.79/mo. for 24 mo.10",
    colorNames: ["Cosmic Orange", "Deep Blue", "Silver"],
    colors: ["#f47a22", "#282420", "#eee2cf"],
    specs: [
      { icon: Cpu, label: "A19 Pro chip" },
      { icon: Camera, label: "Pro Fusion camera system" },
      { icon: BatteryFull, label: "Longest all-day battery" },
    ],
  },
  {
    name: "iPhone Air",
    image: appleAssets.selectAir,
    alt: "iPhone Air in sky blue.",
    tagline: ["The thinnest iPhone ever.", "With the power of pro inside."],
    availability: "",
    price: "From $999 or $41.62/mo. for 24 mo.10",
    colorNames: ["Sky Blue", "Light Gold", "Cloud White", "Space Black"],
    colors: ["#c8d9e4", "#f7f2e4", "#f4f4f5", "#111111"],
    specs: [
      { icon: Smartphone, label: "Ultra-thin enclosure" },
      { icon: Sparkles, label: "Polished titanium feel" },
      { icon: BatteryFull, label: "Efficient all-day battery" },
    ],
  },
  {
    name: "iPhone 17",
    image: appleAssets.select17,
    alt: "iPhone 17 in lavender.",
    tagline: ["Even more delightful.", "Even more durable."],
    availability: "",
    price: "From $799 or $33.29/mo. for 24 mo.10",
    colorNames: ["Lavender", "Sage", "Mist Blue", "White", "Black"],
    colors: ["#c8b8de", "#d9e2cf", "#aebfd2", "#f6f3ec", "#303437"],
    specs: [
      { icon: Cpu, label: "A19 chip" },
      { icon: Camera, label: "Dual Fusion cameras" },
      { icon: ShieldCheck, label: "Tough Ceramic Shield" },
    ],
  },
  {
    eyebrow: "New",
    name: "iPhone 17e",
    image: appleAssets.select17e,
    alt: "iPhone 17e in soft pink.",
    tagline: ["Feature stacked.", "Value packed."],
    availability: "",
    price: "From $599 or $24.95/mo. for 24 mo.10",
    colorNames: ["Soft Pink", "White", "Black"],
    colors: ["#f6d4cf", "#e7edf4", "#f7f4ef"],
    specs: [
      { icon: Cpu, label: "Responsive performance" },
      { icon: Camera, label: "Advanced single camera" },
      { icon: BatteryFull, label: "Big battery life" },
    ],
  },
  {
    name: "iPhone 16",
    image: appleAssets.select16,
    alt: "iPhone 16 in ultramarine.",
    tagline: ["Amazing performance.", "Durable design."],
    availability: "",
    price: "From $699 or $29.12/mo. for 24 mo.10",
    colorNames: ["Ultramarine", "Teal", "Pink", "White", "Black"],
    colors: ["#3157d2", "#a9d4d0", "#f0dae6", "#f6f3ec", "#303437"],
    specs: [
      { icon: Cpu, label: "Fast chip" },
      { icon: Camera, label: "Advanced dual cameras" },
      { icon: ShieldCheck, label: "Durable design" },
    ],
  },
];

export const incentives: VisualCard[] = [
  {
    icon: Smartphone,
    title: "Apple Trade In",
    body: "Save on a new iPhone with a trade-in. Get up to $195–$695 in credit toward iPhone 17, iPhone Air, or iPhone 17 Pro when you trade in iPhone 13 or higher.*",
    image: appleAssets.tradeIn,
    imageSmall: appleAssets.tradeInSmall,
  },
  {
    icon: CreditCard,
    title: "Ways to Buy",
    body: "Pay over time, interest-free. When you choose to check out at Apple with Apple Card Monthly Installments.11",
    image: appleAssets.buy,
    imageSmall: appleAssets.buySmall,
  },
  {
    icon: Headphones,
    title: "Carrier Deals at Apple",
    body: "Get up to $800⁠–⁠ $1100 in credit on a new iPhone after trade‑in.12 Explore deals that accept eligible trade‑in devices in any condition — and some that don’t require a trade‑in at all.",
    image: appleAssets.carrier,
    imageSmall: appleAssets.carrierSmall,
  },
  {
    icon: Sparkles,
    title: "Personal Setup",
    body: "Meet your new iPhone with Personal Setup. Jump into online sessions with a Specialist to set up your iPhone and discover new features.",
    image: appleAssets.setup,
    imageSmall: appleAssets.setupSmall,
  },
  {
    icon: Truck,
    title: "Delivery and Pickup",
    body: "Get flexible delivery and easy pickup. Choose two-hour delivery from an Apple Store, free delivery, or easy pickup options.",
    image: appleAssets.deliver,
    imageSmall: appleAssets.deliverSmall,
  },
  {
    icon: Headphones,
    title: "Guided Shopping",
    body: "Shop live with a Specialist. Let us help you find what you need and answer all of your questions, one on one, at an Apple Store or online.",
    image: appleAssets.specialist,
    imageSmall: appleAssets.specialistSmall,
  },
  {
    icon: ShoppingBag,
    title: "Apple Store App",
    body: "Explore a shopping experience designed around you. Use the Apple Store app to get a more personal way to shop.",
    image: appleAssets.storeApp,
    imageSmall: appleAssets.storeAppSmall,
  },
];

export const considerCards: ConsiderCard[] = [
  {
    eyebrow: "Innovation",
    title: "Beautiful and durable, by design.",
    body: "",
    image: appleAssets.innovation,
    imageSmall: appleAssets.innovationSmall,
    tone: "dark",
  },
  {
    eyebrow: "Cutting-Edge Cameras",
    title: "Picture your best photos and videos.",
    body: "",
    image: appleAssets.camera,
    imageSmall: appleAssets.cameraSmall,
    tone: "dark",
  },
  {
    eyebrow: "Chip and Battery Life",
    title: "Fast that lasts.",
    body: "",
    image: appleAssets.chip,
    imageSmall: appleAssets.chipSmall,
    tone: "dark",
  },
  {
    eyebrow: "iOS and Apple Intelligence",
    title: "New look. Even more magic.",
    body: "",
    image: appleAssets.ios,
    imageSmall: appleAssets.iosSmall,
    tone: "dark",
  },
  {
    eyebrow: "Environment",
    title: "Designed with the earth in mind.",
    body: "",
    image: appleAssets.environment,
    imageSmall: appleAssets.environmentSmall,
    tone: "dark",
  },
  {
    eyebrow: "Privacy",
    title: "Your data. Just where you want it.",
    body: "",
    image: appleAssets.privacyMark,
    imageSmall: appleAssets.privacyMarkSmall,
    tone: "dark",
  },
  {
    eyebrow: "Peace of Mind",
    title: "Helpful features. On and off the grid.",
    body: "",
    image: appleAssets.safety,
    imageSmall: appleAssets.safetySmall,
    tone: "dark",
  },
];

export const essentials: EssentialCard[] = [
  {
    title: "iPhone accessories",
    body: "Protect and personalize your iPhone with fresh accessories like colorful cases, the Crossbody Strap, and more.",
    image: appleAssets.accessories,
    imageSmall: appleAssets.accessoriesSmall,
  },
  {
    eyebrow: "New",
    title: "AirTag",
    body: "Now with a 50% louder speaker and up to a 1.5x greater Precision Finding range,16 it's easier than ever to keep track of what matters.",
    image: appleAssets.airtag,
    imageSmall: appleAssets.airtagSmall,
  },
];

export const companions: CompanionItem[] = [
  {
    name: "Mac",
    title: "iPhone and Mac",
    body: "With iPhone Mirroring, you can view your iPhone screen on your Mac and control it without picking up your phone. Continuity features also let you answer calls or messages right from your Mac. You can even copy images, video, or text from your iPhone and paste it all into a different app on your Mac. And with iCloud, you can access your files from either device.",
    image: appleAssets.mac,
    imageMedium: appleAssets.macMedium,
    imageSmall: appleAssets.macSmall,
  },
  {
    name: "Apple Watch",
    title: "iPhone and Apple Watch",
    body: "Misplaced your iPhone? The latest Apple Watch models can show you its approximate distance and direction.17 To set up a group photo on your iPhone, join the group and use Apple Watch as a viewfinder to snap the shot. And when you take a call on your Apple Watch, just tap your iPhone to continue the conversation there.",
    image: appleAssets.watch,
    imageMedium: appleAssets.watchMedium,
    imageSmall: appleAssets.watchSmall,
  },
  {
    name: "AirPods",
    title: "iPhone and AirPods",
    body: "Set up AirPods on iPhone with just a tap. You’ll love Adaptive Audio, which automatically tailors the noise control for you to provide the best listening experience across different environments and interactions throughout the day.",
    image: appleAssets.airpods,
    imageMedium: appleAssets.airpodsMedium,
    imageSmall: appleAssets.airpodsSmall,
  },
];

export const footerColumns: FooterColumn[] = [
  {
    heading: "Explore iPhone",
    links: [
      "Explore All iPhone",
      "iPhone 17 Pro",
      "iPhone Air",
      "iPhone 17",
      "iPhone 17e",
      "iPhone 16",
      "Compare iPhone",
      "Switch from Android",
    ],
  },
  {
    heading: "Shop iPhone",
    links: [
      "Shop iPhone",
      "iPhone Accessories",
      "Apple Trade In",
      "Carrier Deals at Apple",
      "Financing",
      "Personal Setup",
    ],
  },
  {
    heading: "More from iPhone",
    links: [
      "iPhone Support",
      "AppleCare",
      "iOS 27 Preview",
      "Apple Intelligence",
      "Apps by Apple",
      "iPhone Privacy",
      "Better with Mac",
      "iCloud+",
      "Wallet, Pay, Card",
    ],
  },
];
