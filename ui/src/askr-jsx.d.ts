import type { Props } from '@askrjs/askr';
import type { JSXElement } from '@askrjs/askr/foundations/structures';

declare global {
  namespace JSX {
    interface Element extends JSXElement {
      readonly __askrJsxElementBrand?: never;
    }

    interface IntrinsicElements {
      [elem: string]: Props;
    }

    interface ElementAttributesProperty {
      props: Props;
    }

    interface ElementChildrenAttribute {
      children: unknown;
    }
  }
}