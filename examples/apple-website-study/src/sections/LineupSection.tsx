import { products } from "../content/applePage";
import { ProductCard } from "../components/ProductCard";
import { SectionHeader } from "../components/SectionHeader";

export function LineupSection() {
  return (
    <section className="lineup-section" id="lineup" aria-labelledby="lineup-title">
      <SectionHeader title="Explore the lineup." id="lineup-title" actionLabel="Compare all models" actionHref="#compare" />

      <div className="product-scroller" role="list" aria-label="Product lineup">
        {products.map((product) => (
          <div className="product-scroller__item" role="listitem" key={product.name}>
            <ProductCard product={product} />
          </div>
        ))}
      </div>
    </section>
  );
}
