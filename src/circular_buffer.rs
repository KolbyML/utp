use tracing::info;

impl<T: Clone> Default for SizableCircularBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct SizableCircularBuffer<T> {
    /// This is the mask. Since it's always a power of 2, adding 1 to this value will return the size.
    mask: usize,
    /// This is the elements that the circular buffer points to
    elements: Vec<Option<T>>,
}

impl<T: Clone> SizableCircularBuffer<T> {
    pub fn new() -> Self {
        let mut sizable_circular_buffer = SizableCircularBuffer {
            mask: 15,
            elements: Vec::with_capacity(16),
        };
        // initialize memory with default value of None
        sizable_circular_buffer.elements.resize(16, None);
        sizable_circular_buffer
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        match self.elements.get(i & self.mask) {
            Some(element) => element.as_ref(),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        match self.elements.get_mut(i & self.mask) {
            Some(element) => element.as_mut(),
            _ => None,
        }
    }

    pub fn put(&mut self, index: usize, data: T) {
        self.elements[index & self.mask] = Some(data);
    }

    pub fn delete(&mut self, index: usize) {
        self.elements[index & self.mask] = None;
    }

    // // Item contains the element we want to make space for
    // // index is the index in the list.
    // fn grow(&mut self, item: usize, index: usize) {
    //     // Figure out the new size.
    //     let mut size: usize = self.size();
    //     loop {
    //         size *= 2;
    //         if index < size {
    //             break;
    //         };
    //     }

    //     // Allocate the new vector
    //     let mut new_buffer = Vec::with_capacity(size);
    //     new_buffer.resize(size, None);

    //     size -= 1;

    //     // Copy elements from the old buffer to the new buffer
    //     for i in 0..=self.mask {
    //         let new_index = item.wrapping_sub(index).wrapping_add(i);
    //         match self.get(new_index) {
    //             None => new_buffer[(new_index) & size] = None,
    //             Some(element) => new_buffer[(new_index) & size] = Some((*element).clone()),
    //         }
    //     }

    //     // Swap to the newly allocated buffer
    //     self.mask = size;
    //     self.elements = new_buffer;
    // }

    // pub fn ensure_size(&mut self, item: usize, index: usize) {
    //     if index > self.mask {
    //         self.grow(item, index);
    //     }
    // }

    pub fn ensure_size(&mut self, item: usize, index: usize) {
        if index > self.mask {
            let new_mask = Self::calculate_next_mask(self.mask, index);
            let mut new_items = vec![None; new_mask + 1];
            for i in 0..=self.mask {
                let idx = item.wrapping_sub(index).wrapping_add(i);
                new_items[idx & new_mask] = self.elements[idx & self.mask].take();
            }
            self.elements = new_items;
            self.mask = new_mask;
        }
    }

    fn calculate_next_mask(current_mask: usize, index: usize) -> usize {
        if current_mask == usize::MAX {
            return current_mask;
        }

        let mut new_size = current_mask + 1;
        while new_size != 0 && index >= new_size {
            new_size *= 2;
        }
        new_size - 1
    }

    pub fn size(&self) -> usize {
        self.mask + 1
    }

    pub fn iter(&self) -> impl Iterator<Item = &Option<T>> {
        self.elements.iter()
    }
}
