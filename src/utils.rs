#[derive(Clone, Debug)]
pub struct SizableCircularBuffer<T> {
    // This is the mask. Since it's always a power of 2, adding 1 to this value will return the size.
    mask: usize,
    // This is the elements that the circular buffer points to
    elements: Vec<Option<T>>
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
            Some(&Some(ref element)) => Some(element),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        match self.elements.get_mut(i & self.mask) {
            Some(element) => element.as_mut(),
            _ => None
        }
    }

    pub fn put(&mut self, index: usize, data: T) {
        self.elements[(index & self.mask) as usize] = Some(data);
    }

    pub fn delete(&mut self, index: usize) {
        self.elements[(index & self.mask) as usize] = None;
    }

    // Item contains the element we want to make space for
    // index is the index in the list.
    fn grow(&mut self, item: usize, index: usize) {
        // Figure out the new size.
        let mut size: usize = self.len();
        loop {
            size *= 2;
            if !(index >= size) { break; };
        }

        // Allocate the new vector
        let mut new_buffer = Vec::with_capacity(size);
        new_buffer.resize(size, None);

        size -= 1;

        // Copy elements from the old buffer to the new buffer
        for i in 0..self.mask {
            match self.get(item - index + i) {
                None => new_buffer[(item - index + i) & size] = None,
                Some(element) => new_buffer[(item - index + i) & size] = Some((*element).clone()),
            }
        }

        // Swap to the newly allocated buffer
        self.mask = size;
        self.elements = new_buffer;
    }

    pub fn ensure_size(&mut self, item: usize, index: usize) {
        if index > self.mask {
            self.grow(item, index);
        }
    }

    pub fn len(&self) -> usize {
        self.mask + 1
    }
}

pub fn wrap_compare_less(lhs: u16, rhs: u16) -> bool {
    let dist_down = lhs.wrapping_sub(rhs);
    let dist_up = rhs.wrapping_sub(lhs);
    dist_down < dist_up
}