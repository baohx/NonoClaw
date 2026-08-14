/// 冒泡排序：每一轮把当前未排序区间里最大的元素"冒泡"到末尾。
/// 时间复杂度 O(n^2)，空间复杂度 O(1)，稳定排序。
fn bubble_sort<T: Ord>(arr: &mut [T]) {
    let n = arr.len();
    if n < 2 {
        return;
    }

    // 外层循环控制趟数：n 个元素最多需要 n-1 趟。
    for i in 0..n {
        // 提前退出优化：如果某一趟没有发生交换，说明已经有序。
        let mut swapped = false;

        // 内层循环比较相邻元素，把较大者逐步推到末尾。
        // 每趟结束后，最后 i 个元素已就位，无需再比较。
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
    let mut nums = [5, 2, 9, 1, 7, 3, 8, 4, 6, 0];
    println!("排序前: {:?}", nums);

    bubble_sort(&mut nums);

    println!("排序后: {:?}", nums);
    assert_eq!(nums, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    println!("排序正确 ✓");
}
