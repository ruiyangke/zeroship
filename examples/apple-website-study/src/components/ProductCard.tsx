import { ChevronRight } from "lucide-react";
import { Button, Card } from "@zeroship/ui";
import type { Product } from "../types";

export function ProductCard({ product }: { product: Product }) {
  return (
    <Card className="product-card motion-reveal" variant="ghost" size="lg">
      <Card.Content className="product-card__media">
        <img src={product.image} alt={product.alt} />
      </Card.Content>

      <Card.Header className="product-card__header">
        <div className="product-card__swatches" role="img" aria-label={`${product.name} colors`}>
          {product.colors.map((color) => (
            <span key={color} style={{ backgroundColor: color }} />
          ))}
        </div>
        <span className="product-card__color-names">{product.colorNames.join(" ")}</span>
        {product.eyebrow ? <p className="product-card__eyebrow">{product.eyebrow}</p> : null}
        <Card.Title asChild>
          <h3>{product.name}</h3>
        </Card.Title>
        <Card.Description>
          {product.tagline.map((line, index) => (
            <span key={line}>
              {line}
              {index < product.tagline.length - 1 ? " " : null}
            </span>
          ))}
        </Card.Description>
      </Card.Header>

      <Card.Content className="product-card__body">
        {product.availability ? <p>{product.availability}</p> : null}
        <p className="product-card__price">
          {product.price.replace(/10$/, "")}
          <sup>10</sup>
        </p>
        <div className="product-card__actions">
          <Button asChild>
            <a href="#lineup">Learn more</a>
          </Button>
          <Button asChild variant="plain">
            <a href="#buying">
              Buy
              <ChevronRight aria-hidden="true" size={16} strokeWidth={2} />
            </a>
          </Button>
        </div>
      </Card.Content>
    </Card>
  );
}
