import type { LucideIcon } from "lucide-react";

export interface MegaMenuColumn {
  heading: string;
  elevated?: boolean;
  links: string[];
  secondaryLinks?: string[];
}

export interface MegaMenuModel {
  eyebrow: string;
  columns: MegaMenuColumn[];
}

export interface ChapterItem {
  label: string;
  image: string;
  status: string;
}

export interface ProductSpec {
  icon: LucideIcon;
  label: string;
}

export interface Product {
  eyebrow?: string;
  name: string;
  image: string;
  alt: string;
  tagline: string[];
  availability: string;
  price: string;
  colorNames: string[];
  colors: string[];
  specs: ProductSpec[];
}

export interface VisualCard {
  icon: LucideIcon;
  title: string;
  body: string;
  image: string;
  imageSmall?: string;
}

export interface ConsiderCard {
  eyebrow: string;
  title: string;
  body: string;
  image: string;
  imageSmall?: string;
  tone: "dark" | "light";
}

export interface EssentialCard {
  eyebrow?: string;
  title: string;
  body: string;
  image: string;
  imageSmall?: string;
}

export interface CompanionItem {
  name: string;
  title: string;
  body: string;
  image: string;
  imageMedium: string;
  imageSmall: string;
}

export interface FooterColumn {
  heading: string;
  links: string[];
}
