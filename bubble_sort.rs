/// 冒泡排序（Bubble Sort）
/// 每一轮把未排序区间里的最大值“冒泡”到末尾。
fn bubble_sort<T: Ord>(arr: &mut [T]) {
    let n = arr.len();
    if n <= 1 {
        return;
    }
    for i in 0..n {
        // 提前退出：本轮没有发生交换，说明已经有序
        let mut swapped = false;
        for j in 0..n - 1 - i {
            if arr[j] > arr[j + 1] {
                arr.swap(j, j + 1);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
}

fn main() {
    let mut nums = [5, 2, 9, 1, 5, 6];
    println!("排序前: {:?}", nums);
    bubble_sort(&mut nums);
    println!("排序后: {:?}", nums);

    let mut words = ["pear", "apple", "orange", "banana"];
    bubble_sort(&mut words);
    println!("字符串排序: {:?}", words);
}

#[cfg(test)]
mod tests {
    use super::bubble_sort;

    #[test]
    fn sorts_ints() {
        let mut a = [5, 2, 9, 1, 5, 6];
        bubble_sort(&mut a);
        assert_eq!(a, [1, 2, 5, 5, 6, 9]);
    }

    #[test]
    fn handles_empty_and_single() {
        let mut a: [i32; 0] = [];
        bubble_sort(&mut a);
        assert_eq!(a, []);

        let mut b = [42];
        bubble_sort(&mut b);
        assert_eq!(b, [42]);
    }
}
